"use client";

import { ConversationRouteLink } from "@/components/conversation/ConversationNavigation";
import React, { useCallback, useRef, useEffect, useLayoutEffect, useMemo, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { readChatPreferences } from "@/lib/chatPreferences";
import { cn } from "@/lib/cn";
import { Badge, Button, Callout, EASE_OUT, IconButton, Spinner, Tooltip, toast, useReducedMotion } from "@muteki/ui";
import { formatMessageTime } from "@/components/chat/timeline/MessageActions";
import { MessageCompletionTime } from "@/components/chat/timeline/MessageCompletionTime";
import { ChangedFilesCard } from "@/components/chat/timeline/ChangedFilesCard";
import { SourceChips } from "@/components/chat/timeline/SourceChips";
import { isLocalPreviewUrl } from "@/lib/previewUrlDetect";
import { chatPanel } from "@/lib/chatPanelStore";
import { turnIdFromDiffArtifactName } from "@/lib/conversationDiff";
import { useVirtualizer } from "@muteki/ui/engines/virtual";
import { Chat, LoadingState } from "@muteki/ui/agent";
import { ChatImage } from "@/components/chat/markdown/ChatImage";
import { ConversationMessage, type MessageAnnotationMark, type MessageAttachmentChip } from "./ConversationMessage";
import type { ResourceLinkTarget } from "@/lib/resourcePreview";
import { ConversationTurnProcess } from "./ConversationTurnProcess";
import { TurnContextDialog } from "./TurnContextDialog";
import { buildConversationAgentTree, type ConversationAgentNodeView } from "./conversationAgentTree";
import { ConversationApproval, type UserInputResolvePayload } from "./ConversationApproval";
import { ConversationProposedPlanCard } from "./ConversationPlanPanel";
import { ConversationMinimap, replyExcerpt, type MinimapMarker } from "./ConversationMinimap";
import { ConversationSummaryCard } from "./ConversationSummaryCard";
import { collectConversationSources, extractUrls } from "./conversationSources";
import { Icon } from "@muteki/ui";
import type {
  ConversationCredential,
  ConversationView,
  ConversationEvent,
  ConversationMessage as ConversationMessageRecord,
  ConversationStreamStatus,
} from "@/lib/useConversation";
import { allCredentialModels, fetchTurnProcess, sendConversationCommand } from "@/lib/useConversation";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";
import {
  buildConversationTurnChangesMap,
  buildConversationTurnFold,
  buildConversationTurnRetryMap,
  buildConversationTurnRuntimeMap,
  buildConversationTurnTimingMap,
  conversationErrorMessage,
  EMPTY_TURN_RUNTIME,
  extendConversationTurnFold,
  isAppendedEventList,
  messagePhase,
  presentConversationTurn,
  restoreConversationStreamPrefix,
  resolveConversationLiveActivity,
  type ConversationTurnRuntime,
  type ConversationTurnSegment,
  settleTurnSegmentsAgainstStatus,
} from "./conversationEventViews";
import {
  applyReadingOffset,
  captureVisibleAnchor,
  findAnchorElement,
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
  type ReadingAnchor,
  type TurnExpandState,
} from "@/lib/conversationReadingPosition";
import {
  buildConversationTimelineRows,
  timelineRowIndexForMessageId,
} from "@/lib/conversationTimelineRows";
import { runtimeIdentityKey } from "@/lib/conversationReadingPrefs";
import { useConversationLocalPreview } from "@/lib/conversationPreviewMode";

export interface ConversationTimelineProps {
  view: ConversationView;
  credentials?: ConversationCredential[];
  events?: ConversationEvent[];
  resumedTurnId?: string | null;
  liveText?: string;
  liveTextRuns?: import("@/lib/conversationStreamReducer").ConversationLiveTextRun[];
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
    artifactId?: string;
  }) => void;
  onOpenAttachment?: (attachment: MessageAttachmentChip) => void;
  onResourceLink?: (target: ResourceLinkTarget) => void;
  onApprovalDecision: (approvalId: string, decision: "allow" | "deny", scopeMode?: "once" | "session", optionId?: string, note?: string) => void;
  onUserInputResolve: (payload: UserInputResolvePayload) => void | Promise<boolean>;
  threadId?: string;
  onRetryTurn?: (turnId: string) => void;
  onEditTurn?: (turnId: string, text: string) => void;
  onRewindTurn?: (turnId: string) => void;
  onContinueTurn?: () => void;
  onForkTurn?: (turnId?: string) => void;
  onAskSideChat?: ConversationTimelineProps["onCiteMessageSpan"];
  onCiteMessageSpan?: (payload: {
    messageId: string;
    turnId?: string;
    text: string;
    startOffset: number;
    endOffset: number;
    streaming?: boolean;
    comment?: string;
  }) => void;
  jumpTarget?: {
    key: string;
    messageId: string;
    turnId?: string;
    startOffset?: number;
    endOffset?: number;
  } | null;
  onJumpTargetConsumed?: () => void;
  onJumpToContext?: (payload: {
    messageId: string;
    turnId?: string;
    startOffset?: number;
    endOffset?: number;
  }) => void;
  annotations?: MessageAnnotationMark[];
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
  composerCompact?: boolean;
  onComposerCompactChange?: (compact: boolean) => void;
  className?: string;
  highlightedMessageId?: string;
  suppressRestore?: boolean;
  onEnsureMessageVisible?: (messageId: string) => Promise<"ok" | "missing" | "cancelled" | "superseded">;
  onOpenThread?: (threadId: string) => void;
  turnAccessory?: (turnId: string) => React.ReactNode;
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
      className="cx-animate-in mb-3 overflow-hidden rounded-2xl ring-1 ring-cx-ring bg-cx-elevated shadow-cx-sm"
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
            <h2 className="pt-1 text-cx-base font-semibold leading-5 text-cx-fg">{title}</h2>
            <IconButton icon="x" label="关闭错误提示" noTooltip size="sm" className="-mr-1.5 -mt-0.5" onClick={onDismiss} />
          </div>
          <p className="cx-scroll mt-1 max-h-28 overflow-y-auto whitespace-pre-wrap break-words pr-2 font-cx-mono text-cx-xs leading-5 text-cx-fg-3">
            {detail}
          </p>
        </div>
      </div>
      {(credentialFailure || onContinue) ? (
        <div className="flex flex-wrap items-center justify-end gap-2 border-t border-cx-border-subtle bg-cx-bg-subtle px-4 py-2.5">
          {credentialFailure ? (
            <ConversationRouteLink
              href="/settings/agents"
              className="cx-press inline-flex h-7 items-center gap-1.5 rounded-lg border border-cx-border bg-cx-elevated px-2.5 text-cx-sm font-medium text-cx-fg-2 hover:border-cx-border-strong hover:text-cx-fg"
            >
              <Icon name="plug" size={13} />
              检查 Agent 接入
            </ConversationRouteLink>
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

function TurnErrorCard({
  message,
  retryAt,
  onRetry,
}: {
  message: string;
  retryAt?: string;
  onRetry?: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const [retryCancelled, setRetryCancelled] = useState(false);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const retryMs = retryAt ? Date.parse(retryAt) : NaN;
  const counting = !retryCancelled && Number.isFinite(retryMs) && retryMs > nowMs;
  useEffect(() => {
    if (!counting) return;
    const timer = window.setInterval(() => setNowMs(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [counting]);
  const secondsLeft = counting ? Math.max(0, Math.ceil((retryMs - nowMs) / 1000)) : 0;
  const copy = () => {
    void navigator.clipboard?.writeText(message).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    }).catch(() => {});
  };
  return (
    <section
      className="mt-3 overflow-hidden rounded-xl border border-cx-danger/30 bg-cx-danger-soft"
      role="alert"
      aria-label="错误"
      data-testid="turn-error-card"
    >
      <div className="flex items-center gap-2 px-3 pt-2.5">
        <Icon name="circleAlert" size={14} className="shrink-0 text-cx-danger" />
        <span className="text-cx-sm font-medium text-cx-danger">错误</span>
        <span className="flex-1" />
        <Button size="xs" variant="ghost" icon={copied ? "check" : "copy"} onClick={copy} className="text-cx-fg-3">
          {copied ? "已复制！" : "复制"}
        </Button>
      </div>
      <p className="cx-scroll max-h-40 overflow-y-auto whitespace-pre-wrap break-words px-3 pb-2.5 pt-1.5 font-cx-mono text-cx-xs leading-5 text-cx-fg-2">
        {message}
      </p>
      {onRetry ? (
        <div className="flex items-center justify-end gap-2 border-t border-cx-danger/15 px-3 py-2">
          {counting ? (
            <Button size="xs" variant="ghost" onClick={() => setRetryCancelled(true)} className="text-cx-fg-3">
              取消
            </Button>
          ) : null}
          <Button size="xs" variant="secondary" onClick={onRetry}>
            {counting ? `${secondsLeft} 秒后重试` : "重试"}
          </Button>
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
      className="mb-2 flex min-w-0 items-center gap-2 text-cx-sm leading-5"
      data-testid="c39-identity-full"
    >
      {runtimeText ? (
        <Tooltip content={`运行接入点与模型：${runtimeText}`}>
          <span className="min-w-0 max-w-[260px] truncate font-medium text-cx-fg-2" aria-label={`运行接入点与模型：${runtimeText}`}>
            {runtime?.model || runtime?.endpoint}
          </span>
        </Tooltip>
      ) : (
        <span className="font-medium text-cx-fg-2">Agent</span>
      )}
      {time ? (
        <>
          <span aria-hidden="true" className="-mx-1 text-cx-fg-4">·</span>
          <time dateTime={createdAt} className="cx-tabular text-cx-xs text-cx-fg-4">{time}</time>
        </>
      ) : null}
    </div>
  );
}

function turnIsPlan(turn: { interaction_mode?: string; runtime_snapshot?: { interaction_mode?: string } | null } | undefined): boolean {
  if (!turn) return false;
  const mode = turn.interaction_mode || turn.runtime_snapshot?.interaction_mode;
  return mode === "plan";
}

function AgentInitiatedTurnBadge({ turnId }: { turnId: string }) {
  return (
    <span
      data-testid="agent-initiated-turn-badge"
      data-turn-id={turnId}
      className="mb-1 flex justify-start"
      title="这一回合由 Agent 自行发起（定时唤醒或后台任务完成后续接），不是你发送的消息"
    >
      <Badge tone="neutral" icon="timer">Agent 自动唤醒</Badge>
    </span>
  );
}

function PlanTurnBadge({ turnId, align }: { turnId?: string; align: "start" | "end" }) {
  return (
    <span
      data-testid="plan-turn-badge"
      data-turn-id={turnId || undefined}
      className={cn("mb-1 flex", align === "end" ? "justify-end" : "justify-start")}
    >
      <Badge tone="accent" icon="listChecks">规划</Badge>
    </span>
  );
}

const presentationEvents = new WeakMap<ConversationEvent, ConversationEvent>();
function presentationEvent(event: ConversationEvent): ConversationEvent {
  if (event.stream_seq == null || event.seq === event.stream_seq) return event;
  let normalized = presentationEvents.get(event);
  if (!normalized) {
    normalized = { ...event, seq: event.stream_seq };
    presentationEvents.set(event, normalized);
  }
  return normalized;
}


const COMPOSER_COMPACT_DISTANCE_PX = 200;
const COMPOSER_EXPAND_DISTANCE_PX = 8;
const USER_SCROLL_INPUT_WINDOW_MS = 400;
const HISTORY_OVERSCAN_BUDGET_PX = 560;
const HISTORY_OVERSCAN_HEAVY_ROW_PX = 1600;

type ScrollAnchor = { node: HTMLElement; row: HTMLElement; offset: number };
type PrependAnchor = { threadId: string; firstMessageId?: string; anchor: ReadingAnchor; top: number; settling: boolean };

function layoutOffsetWithin(node: HTMLElement, ancestor: HTMLElement): number | null {
  let offset = 0;
  let current: HTMLElement | null = node;
  while (current && current !== ancestor) {
    offset += current.offsetTop;
    const parent: Element | null = current.offsetParent;
    if (!(parent instanceof HTMLElement) || !ancestor.contains(parent)) return null;
    current = parent;
  }
  return current === ancestor ? offset : null;
}

let nativeScrollAnchoring: boolean | undefined;
function supportsNativeScrollAnchoring(): boolean {
  nativeScrollAnchoring ??= typeof CSS !== "undefined" && CSS.supports("overflow-anchor", "auto");
  return nativeScrollAnchoring;
}

function isNestedScroller(node: HTMLElement): boolean {
  if (node.scrollHeight <= node.clientHeight + 1) return false;
  const overflow = getComputedStyle(node).overflowY;
  return overflow === "auto" || overflow === "scroll" || overflow === "hidden";
}

function firstChildBelow(parent: HTMLElement, top: number, bottom: number): HTMLElement | null {
  for (const child of parent.children) {
    if (!(child instanceof HTMLElement)) continue;
    const rect = child.getBoundingClientRect();
    if (rect.height === 0 || rect.bottom <= top) continue;
    return rect.top < bottom ? child : null;
  }
  return null;
}

function findViewportAnchor(list: HTMLElement, top: number, bottom: number): { node: HTMLElement; row: HTMLElement } | null {
  const row = firstChildBelow(list, top, bottom);
  if (!row) return null;
  let node: HTMLElement | null = null;
  for (let parent = row, depth = 0; depth < 32; depth += 1) {
    const child = firstChildBelow(parent, top, bottom);
    if (!child) break;
    node = child;
    if (child.getBoundingClientRect().top >= top || isNestedScroller(child)) break;
    parent = child;
  }
  return node ? { node, row } : null;
}

function estimateUserBubbleHeight(text: string): number {
  if (!text) return 0;
  if (text.length > 1600) return 220;
  let newlines = 0;
  let pos = 0;
  while ((pos = text.indexOf("\n", pos)) !== -1) {
    newlines += 1;
    if (newlines >= 8) return 220;
    pos += 1;
  }
  return Math.min(220, 56 + Math.max(newlines + 1, Math.ceil(text.length / 38)) * 24);
}

function estimateAssistantMarkdownHeight(text: string): number {
  if (!text) return 140;
  let height = 88;
  let fenceChar = "";
  let fenceSize = 0;
  let fenceVisualize = false;
  let fenceLines = 0;
  let start = 0;
  const total = text.length;
  while (start <= total) {
    const nl = text.indexOf("\n", start);
    const end = nl === -1 ? total : nl;
    let lineEnd = end;
    if (lineEnd > start && text.charCodeAt(lineEnd - 1) === 13) lineEnd -= 1;

    let leading = start;
    while (leading < lineEnd && leading - start < 3 && text.charCodeAt(leading) === 32) {
      leading += 1;
    }
    const firstNonIndent = leading < lineEnd ? text.charCodeAt(leading) : -1;
    let markerLen = 0;
    if (firstNonIndent === 96 || firstNonIndent === 126) {
      let cursor = leading;
      while (cursor < lineEnd && text.charCodeAt(cursor) === firstNonIndent) cursor += 1;
      if (cursor - leading >= 3) markerLen = cursor - leading;
    }

    if (markerLen >= 3) {
      const markerChar = text[leading];
      const restTrimmed = text.slice(leading + markerLen, lineEnd).trim();
      if (!fenceChar) {
        if (firstNonIndent !== 96 || !restTrimmed.includes("`")) {
          fenceChar = markerChar;
          fenceSize = markerLen;
          fenceVisualize = restTrimmed.startsWith("muteki-visualize");
          fenceLines = 0;
        }
      } else if (markerChar === fenceChar && markerLen >= fenceSize && restTrimmed === "") {
        height += fenceVisualize ? 420 : Math.min(600, 64 + fenceLines * 20);
        fenceChar = "";
        fenceSize = 0;
        fenceVisualize = false;
        fenceLines = 0;
      } else {
        fenceLines += 1;
      }
    } else if (fenceChar) {
      fenceLines += 1;
    } else {
      let ws = start;
      while (ws < lineEnd) {
        const code = text.charCodeAt(ws);
        if (code !== 32 && code !== 9) break;
        ws += 1;
      }
      if (ws >= lineEnd) {
        height += 12;
      } else {
        const ch = text.charCodeAt(ws);
        if (ch === 35) height += 44;
        else if (ch === 124) height += 38;
        else height += Math.max(1, Math.ceil((lineEnd - start) / 62)) * 25.5;
      }
    }

    if (nl === -1) break;
    start = nl + 1;
  }
  if (fenceChar) {
    height += Math.min(600, 64 + fenceLines * 20);
  }
  return Math.min(24000, Math.max(140, height));
}

export function ConversationTimeline({
  view,
  credentials = [],
  events = [],
  liveText = "",
  liveTextRuns = [],
  resumedTurnId = null,
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
  onAskSideChat,
  jumpTarget = null,
  onJumpTargetConsumed,
  onJumpToContext,
  annotations,
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
  composerCompact = false,
  onComposerCompactChange,
  className = "",
  highlightedMessageId = "",
  suppressRestore = false,
  onEnsureMessageVisible,
  onOpenThread,
  turnAccessory,
}: ConversationTimelineProps) {
  const streamRef = useRef<HTMLDivElement>(null);
  const composerCompactRef = useRef(composerCompact); composerCompactRef.current = composerCompact;
  const onComposerCompactChangeRef = useRef(onComposerCompactChange); onComposerCompactChangeRef.current = onComposerCompactChange;
  const lastScrollTopRef = useRef(0);
  const highlightedMessageIdRef = useRef(highlightedMessageId); highlightedMessageIdRef.current = highlightedMessageId;
  const virtualListRef = useRef<HTMLDivElement>(null);
  const stickToBottomRef = useRef(true);
  const reducedMotion = useReducedMotion();
  const reducedMotionRef = useRef(reducedMotion); reducedMotionRef.current = reducedMotion;
  const followFrameRef = useRef(0);
  const glidingRef = useRef(false);
  const stopGlide = useCallback(() => {
    cancelAnimationFrame(followFrameRef.current);
    glidingRef.current = false;
  }, []);
  const followLiveEdge = useCallback((element: HTMLElement, instant = false) => {
    const contentHeight = element.scrollHeight;
    const viewportHeight = element.clientHeight;
    const gap = contentHeight - viewportHeight - element.scrollTop;
    if (instant || reducedMotionRef.current || gap > viewportHeight * 1.5) {
      stopGlide();
      element.scrollTop = contentHeight;
      return;
    }
    if (glidingRef.current || gap <= 0.5) return;
    glidingRef.current = true;
    const step = () => {
      const remaining = element.scrollHeight - element.clientHeight - element.scrollTop;
      if (!stickToBottomRef.current || remaining <= 0.5) {
        glidingRef.current = false;
        return;
      }
      element.scrollTop += Math.max(1, remaining * 0.2);
      followFrameRef.current = requestAnimationFrame(step);
    };
    followFrameRef.current = requestAnimationFrame(step);
  }, [stopGlide]);
  useEffect(() => stopGlide, [stopGlide]);
  const loadingOlderRef = useRef(false);
  const pendingAnchorRef = useRef<{ threadId: string; firstMessageId?: string; succeeded: boolean } | null>(null);
  const prevVirtualListTopRef = useRef<{ threadId: string; top: number } | null>(null);
  const scrollRequestRef = useRef(0);
  const [anchorCommit, setAnchorCommit] = useState(0);
  const activeThreadRef = useRef(view.thread.thread_id); activeThreadRef.current = view.thread.thread_id;
  const highlightScrollDoneRef = useRef("");
  const restoreDoneRef = useRef("");
  const restoringAnchorRef = useRef(false);
  const anchorRequestRef = useRef(0);
  const lastAnchorRef = useRef<ReadingAnchor | null>(null);
  const lastUserScrollAtRef = useRef(0);
  const lastUserInputAtRef = useRef(0);
  const detachedFromLiveEdgeRef = useRef(false);
  const anchorPersistTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const isVirtualizerScrollingRef = useRef<(() => boolean) | null>(null);
  const expandByTurnRef = useRef<TurnExpandState>({});
  const [showJumpToLatest, setShowJumpToLatest] = useState(false);
  const [hasJumpBack, setHasJumpBack] = useState(() => Boolean(peekJumpBack()));
  const [dismissedFailureKey, setDismissedFailureKey] = useState("");
  const [sentContextTurnId, setSentContextTurnId] = useState<string | null>(null);
  const [hydratedEventsByTurn, setHydratedEventsByTurn] = useState<
    Record<string, ConversationEvent[]>
  >({});
  const hydratedRef = useRef(hydratedEventsByTurn); hydratedRef.current = hydratedEventsByTurn;
  const [processByTurn, setProcessByTurn] = useState<Record<string, {
    loading: boolean; error: string; hasMore: boolean;
    nextAfterSeq?: number | null; nextBeforeSeq?: number | null; watermark?: number;
  }>>({});
  const processRef = useRef(processByTurn); processRef.current = processByTurn;
  const localPreview = useConversationLocalPreview();
  const processRequestsRef = useRef(new Map<string, AbortController>());
  const [expandByTurn, setExpandByTurn] = useState<TurnExpandState>({});

  useEffect(() => {
    expandByTurnRef.current = expandByTurn;
  }, [expandByTurn]);

  useEffect(() => subscribeJumpBack(() => setHasJumpBack(Boolean(peekJumpBack()))), []);

  const initializedThreadRef = useRef<string | null>(null);
  useEffect(() => {
    const threadKey = view.thread.thread_id;
    const requests = processRequestsRef.current;
    if (initializedThreadRef.current !== threadKey) {
      initializedThreadRef.current = threadKey;
      setHydratedEventsByTurn({});
      setProcessByTurn({});
      for (const controller of requests.values()) controller.abort();
      requests.clear();
      scrollRequestRef.current += 1;
      pendingAnchorRef.current = null;
      prevVirtualListTopRef.current = null;
      if (anchorPersistTimerRef.current) {
        clearTimeout(anchorPersistTimerRef.current);
        anchorPersistTimerRef.current = null;
      }
      loadingOlderRef.current = false;
      setExpandByTurn(readExpandState(threadKey));
      restoreDoneRef.current = "";
      restoringAnchorRef.current = false;
      anchorRequestRef.current += 1;
      lastAnchorRef.current = null;
      setDismissedFailureKey("");
    } else {
      setProcessByTurn(previous => Object.fromEntries(Object.entries(previous).map(([id, value]) => [id, value.loading ? { ...value, loading: false } : value])));
    }
    return () => {
      for (const controller of requests.values()) controller.abort();
      requests.clear();
      scrollRequestRef.current += 1;
      pendingAnchorRef.current = null;
      if (anchorPersistTimerRef.current) {
        clearTimeout(anchorPersistTimerRef.current);
        anchorPersistTimerRef.current = null;
      }
    };
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

  const persistCurrentAnchor = useCallback((
    element: HTMLElement,
    stickToBottom: boolean,
  ) => {
    if (anchorPersistTimerRef.current) {
      clearTimeout(anchorPersistTimerRef.current);
      anchorPersistTimerRef.current = null;
    }
    const threadKey = view.thread.thread_id;
    if (!threadKey) return;
    if (suppressRestore) return;
    if (restoringAnchorRef.current) return;
    const captured = captureVisibleAnchor(element);
    if (captured) lastAnchorRef.current = captured;
    const anchor = captured ?? lastAnchorRef.current;
    if (!anchor) return;
    writeReadingPosition({
      threadId: threadKey,
      ...anchor,
      stickToBottom,
      expandByTurn: expandByTurnRef.current,
    });
  }, [suppressRestore, view.thread.thread_id]);

  const schedulePersistCurrentAnchor = useCallback(() => {
    if (anchorPersistTimerRef.current) clearTimeout(anchorPersistTimerRef.current);
    anchorPersistTimerRef.current = setTimeout(() => {
      anchorPersistTimerRef.current = null;
      const element = streamRef.current;
      if (!element) return;
      if (restoringAnchorRef.current) return;
      if (String(highlightedMessageIdRef.current || "").trim()) return;
      persistCurrentAnchor(element, stickToBottomRef.current);
    }, 150);
  }, [persistCurrentAnchor]);

  const reapplySavedAnchor = useCallback(() => {
    if (suppressRestore) return;
    if (restoringAnchorRef.current) return;
    if (stickToBottomRef.current) return;
    if (String(highlightedMessageId || "").trim()) return;
    if (loadingOlderRef.current || pendingAnchorRef.current) return;
    const element = streamRef.current;
    const saved = lastAnchorRef.current || readReadingPosition(view.thread.thread_id);
    if (!element || !saved?.messageId) return;
    applyReadingOffset(element, saved);
  }, [highlightedMessageId, suppressRestore, view.thread.thread_id]);

  const rowVirtualizerRef = useRef<{ scrollOffset: number | null } | null>(null);
  const scrollStreamBy = useCallback((element: HTMLElement, delta: number) => {
    element.scrollTop += delta;
    const virtualizer = rowVirtualizerRef.current;
    if (virtualizer && virtualizer.scrollOffset !== null) virtualizer.scrollOffset = element.scrollTop;
  }, []);
  const scrollAnchorRef = useRef<ScrollAnchor | null>(null);
  const anchorRowObserverRef = useRef<ResizeObserver | null>(null);
  const pickScrollAnchor = useCallback(() => {
    const element = streamRef.current;
    const list = virtualListRef.current;
    const previousRow = scrollAnchorRef.current?.row ?? null;
    let next: ScrollAnchor | null = null;
    if (element && list && !supportsNativeScrollAnchoring()) {
      const top = element.getBoundingClientRect().top;
      const found = findViewportAnchor(list, top, top + element.clientHeight);
      const offset = found && layoutOffsetWithin(found.node, found.row);
      if (found && offset !== null) next = { ...found, offset };
    }
    scrollAnchorRef.current = next;
    const observer = anchorRowObserverRef.current;
    if (observer && previousRow !== (next?.row ?? null)) {
      if (previousRow) observer.unobserve(previousRow);
      if (next) observer.observe(next.row);
    }
  }, []);
  const correctScrollAnchor = useCallback(() => {
    const anchor = scrollAnchorRef.current;
    const element = streamRef.current;
    if (!anchor || !element) return;
    const offset = anchor.row.isConnected && anchor.node.getClientRects().length
      ? layoutOffsetWithin(anchor.node, anchor.row)
      : null;
    if (offset === null) {
      pickScrollAnchor();
      return;
    }
    const drift = offset - anchor.offset;
    anchor.offset = offset;
    if (Math.abs(drift) < 1) return;
    if (
      stickToBottomRef.current
      || restoringAnchorRef.current
      || loadingOlderRef.current
      || pendingAnchorRef.current
      || prependAnchorRef.current?.settling
    ) return;
    if (String(highlightedMessageIdRef.current || "").trim()) return;
    scrollStreamBy(element, drift);
  }, [pickScrollAnchor, scrollStreamBy]);
  useEffect(() => {
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => correctScrollAnchor());
    anchorRowObserverRef.current = observer;
    const row = scrollAnchorRef.current?.row;
    if (row) observer.observe(row);
    return () => {
      observer.disconnect();
      anchorRowObserverRef.current = null;
    };
  }, [correctScrollAnchor]);

  const prependAnchorRef = useRef<PrependAnchor | null>(null);
  const prependObserverRef = useRef<ResizeObserver | null>(null);
  const prependSettleTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const readPrependAnchorTop = useCallback((anchor: ReadingAnchor): number | null => {
    const element = streamRef.current;
    const list = virtualListRef.current;
    const node = element && list ? findAnchorElement(element, anchor) : null;
    const offset = node && list ? layoutOffsetWithin(node, list) : null;
    if (!element || !list || offset === null) return null;
    return list.getBoundingClientRect().top - element.getBoundingClientRect().top + offset;
  }, []);
  const capturePrependAnchor = useCallback((threadId: string, firstMessageId: string | undefined) => {
    const anchor = captureVisibleAnchor(streamRef.current);
    const top = anchor ? readPrependAnchorTop(anchor) : null;
    prependAnchorRef.current = anchor && top !== null
      ? { threadId, firstMessageId, anchor, top, settling: false }
      : null;
  }, [readPrependAnchorTop]);
  const releasePrependAnchor = useCallback(() => {
    prependAnchorRef.current = null;
    prependObserverRef.current?.disconnect();
    if (prependSettleTimerRef.current) clearTimeout(prependSettleTimerRef.current);
    prependSettleTimerRef.current = null;
  }, []);
  const holdPrependAnchor = useCallback(() => {
    const pinned = prependAnchorRef.current;
    const element = streamRef.current;
    if (!pinned?.settling || !element) return;
    const top = readPrependAnchorTop(pinned.anchor);
    if (top === null) {
      releasePrependAnchor();
      return;
    }
    const drift = top - pinned.top;
    if (Math.abs(drift) >= 1) scrollStreamBy(element, drift);
    if (prependSettleTimerRef.current) clearTimeout(prependSettleTimerRef.current);
    prependSettleTimerRef.current = setTimeout(releasePrependAnchor, 800);
  }, [readPrependAnchorTop, releasePrependAnchor, scrollStreamBy]);
  useEffect(() => {
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => holdPrependAnchor());
    prependObserverRef.current = observer;
    return () => {
      releasePrependAnchor();
      observer.disconnect();
      prependObserverRef.current = null;
    };
  }, [holdPrependAnchor, releasePrependAnchor]);

  const updateReaderPosition = useCallback((distanceFromBottom: number) => {
    const nearBottom = glidingRef.current || distanceFromBottom < 96;
    setShowJumpToLatest(!nearBottom);
    return nearBottom;
  }, []);

  const handleStreamScroll = useCallback((event: React.UIEvent<HTMLDivElement>) => {
    const element = event.currentTarget;
    const scrollTop = element.scrollTop;
    const scrollHeight = element.scrollHeight;
    const distance = scrollHeight - scrollTop - element.clientHeight;
    const nearBottom = updateReaderPosition(distance);
    const previousTop = lastScrollTopRef.current;
    lastScrollTopRef.current = scrollTop;
    const pinned = prependAnchorRef.current;
    if (pinned?.settling) {
      const top = readPrependAnchorTop(pinned.anchor);
      if (top === null) releasePrependAnchor();
      else pinned.top = top;
    } else if (pinned && pendingAnchorRef.current) {
      capturePrependAnchor(pinned.threadId, pinned.firstMessageId);
    }
    const onCompactChange = onComposerCompactChangeRef.current;
    if (onCompactChange) {
      if (!composerCompactRef.current && scrollTop < previousTop - 1 && distance > COMPOSER_COMPACT_DISTANCE_PX) {
        onCompactChange(true);
      } else if (composerCompactRef.current && scrollTop > previousTop && distance <= COMPOSER_EXPAND_DISTANCE_PX) {
        onCompactChange(false);
      }
    }
    if (restoringAnchorRef.current) return;
    lastUserScrollAtRef.current = performance.now();
    const userDriven = performance.now() - lastUserInputAtRef.current < USER_SCROLL_INPUT_WINDOW_MS;
    if (userDriven && scrollTop < previousTop - 0.5) detachedFromLiveEdgeRef.current = true;
    else if (userDriven && scrollTop > previousTop && nearBottom) detachedFromLiveEdgeRef.current = false;
    stickToBottomRef.current = nearBottom && !detachedFromLiveEdgeRef.current;
    pickScrollAnchor();
    if (!String(highlightedMessageId || "").trim()) {
      schedulePersistCurrentAnchor();
    }
    if (String(highlightedMessageId || "").trim()) return;
    if (
      onLoadOlder
      && view.messages_page?.has_more_before
      && !loadingOlderRef.current
      && scrollTop < 120
    ) {
      loadingOlderRef.current = true;
      stickToBottomRef.current = false;
      pendingAnchorRef.current = {
        threadId: view.thread.thread_id,
        firstMessageId: view.messages[0]?.message_id,
        succeeded: false,
      };
      const anchor = pendingAnchorRef.current;
      const request = scrollRequestRef.current;
      capturePrependAnchor(anchor.threadId, anchor.firstMessageId);
      const abandon = () => {
        pendingAnchorRef.current = null;
        if (!prependAnchorRef.current?.settling) releasePrependAnchor();
      };
      void Promise.resolve(onLoadOlder()).then((loaded) => {
        if (pendingAnchorRef.current !== anchor) return;
        if (!loaded || activeThreadRef.current !== anchor.threadId || scrollRequestRef.current !== request) abandon();
        else { anchor.succeeded = true; setAnchorCommit(value => value + 1); }
      }).catch(() => { if (pendingAnchorRef.current === anchor) abandon(); })
        .finally(() => { if (activeThreadRef.current === anchor.threadId && scrollRequestRef.current === request) loadingOlderRef.current = false; });
    }
  }, [
    capturePrependAnchor,
    highlightedMessageId,
    onLoadOlder,
    pickScrollAnchor,
    readPrependAnchorTop,
    releasePrependAnchor,
    schedulePersistCurrentAnchor,
    updateReaderPosition,
    view.messages_page?.has_more_before,
    view.thread.thread_id,
    view.messages,
  ]);

  useEffect(() => {
    loadingOlderRef.current = loadingOlder;
  }, [loadingOlder]);

  const jumpToLatest = useCallback(() => {
    const request = ++scrollRequestRef.current;
    const threadId = view.thread.thread_id;
    if (anchorPersistTimerRef.current) {
      clearTimeout(anchorPersistTimerRef.current);
      anchorPersistTimerRef.current = null;
    }
    void Promise.resolve(onJumpToLatest?.() ?? true).then((loaded) => {
      if (!loaded || request !== scrollRequestRef.current || activeThreadRef.current !== threadId) return;
      pendingAnchorRef.current = null;
      releasePrependAnchor();
      stickToBottomRef.current = true;
      detachedFromLiveEdgeRef.current = false;
      setShowJumpToLatest(false);
      onComposerCompactChangeRef.current?.(false);
      const scrollTip = () => {
        if (request !== scrollRequestRef.current || activeThreadRef.current !== threadId) return;
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
    }).catch(() => { if (request === scrollRequestRef.current) setShowJumpToLatest(true); });
  }, [onJumpToLatest, persistCurrentAnchor, releasePrependAnchor, view.thread.thread_id]);


  useEffect(() => {
    const sent = (event: Event) => {
      if ((event as CustomEvent).detail?.threadId === view.thread.thread_id && readChatPreferences().scrollToLatestOnSend) jumpToLatest();
    };
    window.addEventListener("muteki:conversation-sent", sent);
    return () => window.removeEventListener("muteki:conversation-sent", sent);
  }, [jumpToLatest, view.thread.thread_id]);

  const readingVisitRef = useRef<{ threadId: string; suppressRestore: boolean } | null>(null);
  useEffect(() => {
    const threadKey = view.thread.thread_id;
    if (readingVisitRef.current?.threadId === threadKey && readingVisitRef.current.suppressRestore === suppressRestore) return;
    readingVisitRef.current = { threadId: threadKey, suppressRestore };
    detachedFromLiveEdgeRef.current = false;
    if (suppressRestore) {
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
      elementMessageId: saved.elementMessageId,
    };
  }, [suppressRestore, view.thread.thread_id]);

  useEffect(() => {
    if (!stickToBottomRef.current || loadingOlder) return;
    if (String(highlightedMessageId || "").trim()) return;
    const frame = requestAnimationFrame(() => {
      const element = streamRef.current;
      if (!element) return;
      followLiveEdge(element);
    });
    return () => cancelAnimationFrame(frame);
  }, [
    followLiveEdge,
    view.messages.length,
    highlightedMessageId,
    liveText.length,
    running,
    events.length,
    loadingOlder,
  ]);

  useEffect(() => {
    if (stickToBottomRef.current) return;
    const frame = requestAnimationFrame(() => reapplySavedAnchor());
    return () => cancelAnimationFrame(frame);
  }, [expandByTurn, reapplySavedAnchor]);

  useEffect(() => {
    const element = streamRef.current;
    if (!element || typeof ResizeObserver === "undefined") return;
    let frame = 0;
    let viewportResized = false;
    let viewportReflowed = false;
    let viewportWidth = element.clientWidth;
    const update = () => {
      const activelyScrolling = Boolean(isVirtualizerScrollingRef.current?.())
        || (performance.now() - lastUserScrollAtRef.current < 250);
      if (viewportReflowed || (!activelyScrolling && !pendingAnchorRef.current)) {
        reapplySavedAnchor();
      }
      if (
        stickToBottomRef.current
        && !restoringAnchorRef.current
        && !loadingOlderRef.current
        && !String(highlightedMessageIdRef.current || "").trim()
      ) {
        followLiveEdge(element, viewportResized);
      }
      const scrollRange = element.scrollHeight - element.clientHeight;
      updateReaderPosition(scrollRange - element.scrollTop);
      if (composerCompactRef.current && scrollRange <= COMPOSER_EXPAND_DISTANCE_PX) {
        onComposerCompactChangeRef.current?.(false);
      }
      viewportResized = false;
      viewportReflowed = false;
    };
    const observer = new ResizeObserver((entries) => {
      if (entries.some((entry) => entry.target === element)) {
        viewportResized = true;
        const width = element.clientWidth;
        if (width !== viewportWidth) {
          viewportWidth = width;
          viewportReflowed = true;
        }
      }
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(update);
    });
    const content = element.firstElementChild;
    if (content) observer.observe(content);
    observer.observe(element);
    return () => { observer.disconnect(); cancelAnimationFrame(frame); };
  }, [followLiveEdge, reapplySavedAnchor, updateReaderPosition, view.thread.thread_id]);

  useEffect(() => {
    const element = streamRef.current;
    if (!element) return;
    const handleWheel = (event: WheelEvent) => {
      lastUserScrollAtRef.current = performance.now();
      lastUserInputAtRef.current = lastUserScrollAtRef.current;
      if (event.deltaY < 0) stopGlide();
      if (event.deltaY < 0 && element.scrollTop > 0) {
        detachedFromLiveEdgeRef.current = true;
        stickToBottomRef.current = false;
      }
      if (!composerCompactRef.current || event.deltaY <= 0) return;
      if (element.scrollHeight - element.scrollTop - element.clientHeight > COMPOSER_EXPAND_DISTANCE_PX) return;
      onComposerCompactChangeRef.current?.(false);
    };
    const handlePointerOrTouch = () => {
      lastUserScrollAtRef.current = performance.now();
      lastUserInputAtRef.current = lastUserScrollAtRef.current;
      stopGlide();
    };
    const handleInput = () => { lastUserInputAtRef.current = performance.now(); };
    element.addEventListener("wheel", handleWheel, { passive: true });
    element.addEventListener("touchstart", handlePointerOrTouch, { passive: true });
    element.addEventListener("pointerdown", handlePointerOrTouch, { passive: true });
    element.addEventListener("touchmove", handleInput, { passive: true });
    element.addEventListener("keydown", handleInput, { passive: true });
    return () => {
      element.removeEventListener("wheel", handleWheel);
      element.removeEventListener("touchstart", handlePointerOrTouch);
      element.removeEventListener("pointerdown", handlePointerOrTouch);
      element.removeEventListener("touchmove", handleInput);
      element.removeEventListener("keydown", handleInput);
    };
  }, [stopGlide, view.thread.thread_id]);

  const persistedAssistantEvents = useMemo(() => {
    const lastMessageByTurn = new Map<string, ConversationMessageRecord>();
    for (const message of view.messages) {
      if (message.role === "assistant" && message.turn_id) lastMessageByTurn.set(message.turn_id, message);
    }
    const statuses = new Map(view.turns.map(turn => [turn.turn_id, turn.status]));
    const result = new Map<string, ConversationEvent>();
    for (const message of view.messages) {
      if (message.role !== "assistant" || !message.turn_id || message.stream_seq == null) continue;
      const status = statuses.get(message.turn_id);
      const settled = ["completed", "failed", "interrupted", "aborted", "cancelled", "canceled"].includes(status || "");
      if (!settled && message.status !== "completed") continue;
      result.set(message.message_id, {
        event_id: `persisted:${message.message_id}`,
        event_type: "core.message.completed",
        seq: message.stream_seq,
        stream_seq: message.stream_seq,
        occurred_at: message.created_at,
        payload: {
          turn_id: message.turn_id, message_id: message.message_id,
          role: "assistant", text: message.text,
          phase: (message.phase !== "unknown" && message.phase) || (status === "completed" && lastMessageByTurn.get(message.turn_id) === message
            ? "final_answer" : "commentary"),
        },
      });
    }
    return result;
  }, [view.messages, view.turns]);

  const foldEvents = useMemo(() => {
    const restored = new Map(persistedAssistantEvents);
    const byKey = new Map<string, ConversationEvent>();
    const recordedToolCalls = new Set<string>();
    const admit = (raw: ConversationEvent) => {
      const event = presentationEvent(raw);
      if (event.seq > 0 && event.event_type.startsWith("core.tool.") && event.payload.tool_call_id) {
        recordedToolCalls.add(JSON.stringify([event.payload.turn_id, event.payload.tool_call_id]));
      }
      const messageId = String(event.payload.message_id || "");
      const snapshot = restored.get(messageId);
      if (snapshot && (event.event_type === "core.message.delta" || event.event_type === "core.message.completed")
        && event.payload.thinking !== true) {
        const phase = messagePhase(event.payload);
        if (phase !== "unknown" && snapshot.payload.phase !== phase) {
          restored.set(messageId, { ...snapshot, payload: { ...snapshot.payload, phase } });
        }
        return;
      }
      byKey.set(String(event.event_id || `${event.seq}:${event.event_type}`), event);
    };
    for (const rows of Object.values(hydratedEventsByTurn)) for (const event of rows) admit(event);
    for (const event of events) admit(event);
    for (const event of restored.values()) byKey.set(event.event_id!, event);
    return Array.from(byKey.values()).filter((event) => (
      !event.event_id?.startsWith("persisted-tool:")
      || !recordedToolCalls.has(JSON.stringify([event.payload.turn_id, event.payload.tool_call_id]))
    )).sort((a, b) => a.seq - b.seq);
  }, [events, hydratedEventsByTurn, persistedAssistantEvents]);

  const foldCacheRef = useRef<{
    events: ConversationEvent[];
    persisted: typeof persistedAssistantEvents;
    hydrated: typeof hydratedEventsByTurn;
    fold: ReturnType<typeof buildConversationTurnFold>;
  } | null>(null);
  const turnFold = useMemo(() => {
    const previous = foldCacheRef.current;
    const fold = previous
      && previous.persisted === persistedAssistantEvents
      && previous.hydrated === hydratedEventsByTurn
      && isAppendedEventList(previous.events, foldEvents)
      ? extendConversationTurnFold(previous.fold, foldEvents.slice(previous.events.length))
      : buildConversationTurnFold(foldEvents);
    foldCacheRef.current = { events: foldEvents, persisted: persistedAssistantEvents, hydrated: hydratedEventsByTurn, fold };
    return fold;
  }, [foldEvents, persistedAssistantEvents, hydratedEventsByTurn]);
  const agentTree = useMemo(() => buildConversationAgentTree(foldEvents, view), [foldEvents, view]);
  const agentRootsByTurn = useMemo(() => {
    const map = new Map<string, ConversationAgentNodeView[]>();
    for (const root of agentTree.roots) {
      if (!root.turnId) continue;
      const list = map.get(root.turnId) || [];
      list.push(root);
      map.set(root.turnId, list);
    }
    return map;
  }, [agentTree]);
  const conversationSources = useMemo(
    () => collectConversationSources(foldEvents, view.messages),
    [foldEvents, view.messages],
  );
  const runtimeByTurn = useMemo(
    () => buildConversationTurnRuntimeMap(foldEvents),
    [foldEvents],
  );
  const timingByTurn = useMemo(
    () => buildConversationTurnTimingMap(foldEvents),
    [foldEvents],
  );
  const retryByTurn = useMemo(
    () => buildConversationTurnRetryMap(foldEvents),
    [foldEvents],
  );
  const changesByTurn = useMemo(
    () => buildConversationTurnChangesMap(foldEvents),
    [foldEvents],
  );

  const ensureTurnProcess = useCallback(async (turnId: string, more = false) => {
    const previous = processRef.current[turnId];
    if (!turnId || processRequestsRef.current.has(turnId)) return;
    if (!more && hydratedRef.current[turnId] && !previous?.error) return;
    if (more && previous && !previous.hasMore && !previous.error) return;
    if (localPreview) {
      setProcessByTurn(current => ({ ...current, [turnId]: { loading: false, error: "", hasMore: false } }));
      return;
    }
    const threadId = view.thread.thread_id;
    const controller = new AbortController(); processRequestsRef.current.set(turnId, controller);
    setProcessByTurn(current => ({ ...current, [turnId]: { ...current[turnId], loading: true, error: "", hasMore: previous?.hasMore ?? false } }));
    try {
      const cursor = more ? {
        afterSeq: previous?.nextAfterSeq ?? undefined,
        beforeSeq: previous?.nextBeforeSeq ?? undefined,
        watermark: previous?.watermark,
      } : turnId === resumedTurnId && turnId === view.state.running_turn_id ? {
        beforeSeq: view.watermark + 1,
      } : {};
      const detail = await fetchTurnProcess(threadId, turnId, { ...cursor, signal: controller.signal });
      if (controller.signal.aborted || activeThreadRef.current !== threadId) return;
      setHydratedEventsByTurn((current) => ({
        ...current,
        [turnId]: Array.from(new Map([...(more ? current[turnId] || [] : []), ...detail.events].map(event => [event.event_id || `${event.seq}:${event.event_type}`, event])).values()).sort((a,b) => a.seq - b.seq),
      }));
      setProcessByTurn(current => ({ ...current, [turnId]: { loading: false, error: "",
        hasMore: detail.has_more, nextAfterSeq: detail.next_after_seq, nextBeforeSeq: detail.next_before_seq,
        watermark: detail.watermark } }));
    } catch (error) {
      if (controller.signal.aborted || activeThreadRef.current !== threadId) return;
      setProcessByTurn(current => ({ ...current, [turnId]: { ...current[turnId], loading: false,
        hasMore: current[turnId]?.hasMore ?? false, error: error instanceof Error ? error.message : String(error) } }));
    } finally {
      if (processRequestsRef.current.get(turnId) === controller) processRequestsRef.current.delete(turnId);
    }
  }, [localPreview, resumedTurnId, view.thread.thread_id, view.state.running_turn_id, view.watermark]);

  const turnById = useMemo(
    () => new Map(view.turns.map((turn) => [turn.turn_id, turn])),
    [view.turns],
  );

  const rawPendingApprovals = view.state.pending_approvals || {};
  const pendingApprovals = Object.fromEntries(
    Object.entries(rawPendingApprovals).filter(([, row]) => {
      if (!row || typeof row !== "object") return false;
      const status = String((row as Record<string, unknown>).status || "pending");
      return status === "pending" || status === "resolving" || status === "expired";
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

  const pendingApprovalTotal = new Set([
    ...Object.entries(pendingApprovals)
      .filter(([, row]) => String(row.status || "pending") === "pending")
      .map(([id]) => id),
    ...(pendingApproval && String(pendingApproval.status || "pending") === "pending" ? [String(pendingApproval.approval_id)] : []),
  ]).size;
  const attentionLabel = pendingApprovalTotal
    ? `${pendingApprovalTotal} 个操作等待审批`
    : pendingInput ? "Agent 等待你的回复" : "";

  const artifacts = useMemo(() => view.artifacts || [], [view.artifacts]);
  const diffArtifactByTurn = useMemo(() => {
    const byTurn = new Map<string, (typeof artifacts)[number]>();
    for (const artifact of artifacts) {
      if (!artifact.artifact_id || (artifact.kind !== "conversation.diff" && !artifact.name?.endsWith(".diff"))) continue;
      const turnId = (artifact as { turn_id?: string }).turn_id;
      const namedTurnId = turnIdFromDiffArtifactName(artifact.name);
      if (turnId && !byTurn.has(turnId)) byTurn.set(turnId, artifact);
      if (namedTurnId && !byTurn.has(namedTurnId)) byTurn.set(namedTurnId, artifact);
    }
    return byTurn;
  }, [artifacts]);
  const generatedImagesByTurn = useMemo(() => {
    const turns = new Map<string, typeof artifacts>();
    for (const image of artifacts) {
      if (image.kind !== "conversation.generated_image" || !image.artifact_id || !image.turn_id) continue;
      const images = turns.get(image.turn_id);
      if (images) images.push(image);
      else turns.set(image.turn_id, [image]);
    }
    return turns;
  }, [artifacts]);

  const artifactsById = useMemo(() => {
    const map = new Map<string, (typeof artifacts)[number]>();
    for (const item of artifacts) {
      if (item.artifact_id) map.set(item.artifact_id, item);
      if (item.sha256) map.set(item.sha256, item);
    }
    return map;
  }, [artifacts]);

  const resolveAttachments = useCallback((hashes: string[]): MessageAttachmentChip[] => {
    return hashes.map((sha) => {
      const matched = artifactsById.get(sha);
      const current = Boolean(matched?.artifact_id);
      return {
        artifactId: matched?.artifact_id || undefined,
        needsReselect: !current,
        name: matched?.name || sha.slice(0, 12),
        size: matched?.size,
        mediaType: matched?.media_type,
        kind: matched?.kind,
      };
    });
  }, [artifactsById]);

  const lastTurn = view.turns.at(-1);
  const lastTurnTerminal = String(lastTurn?.status || "").toLowerCase();
  const isLastTurnFailed = lastTurnTerminal === "failed";
  const isLastTurnInterrupted = ["interrupted", "aborted"].includes(lastTurnTerminal);
  const isLastTurnCancelled = ["cancelled", "canceled"].includes(lastTurnTerminal);
  const failureMessage = conversationErrorMessage(lastTurn?.error)
    || conversationErrorMessage(view.state.last_error)
    || (isLastTurnCancelled
      ? "本轮已取消，已有消息和工作记录已保留。可以发送新消息或重试。"
      : isLastTurnInterrupted
      ? "本轮已停止，已有消息和工作记录已保留。可以继续执行。"
      : "Runtime 未返回具体错误信息");
  const {
    assistantMessagesByTurn,
    assistantPartsByTurn,
    assistantTextByTurn,
    assistantMessageIdByTurn,
    messageTurnIds,
    lastPromptIndexByTurn,
    promptMessageIndexes,
    userPromptIndexesByTurn,
  } = useMemo(() => {
    const assistantMessagesByTurn = new Map<string, ConversationMessageRecord>();
    const assistantPartsByTurn = new Map<string, ConversationMessageRecord[]>();
    const assistantTextByTurn = new Map<string, string>();
    const assistantMessageIdByTurn = new Map<string, string>();
    const messageTurnIds = new Set<string>();
    const lastPromptIndexByTurn = new Map<string, number>();
    const promptMessageIndexes: number[] = [];
    const userPromptIndexesByTurn = new Map<string, number[]>();
    view.messages.forEach((message, index) => {
      const turnId = message.turn_id;
      if (!turnId) return;
      if (message.role === "assistant") {
        assistantMessagesByTurn.set(turnId, message);
        if (message.message_id) assistantMessageIdByTurn.set(turnId, message.message_id);
        else assistantMessageIdByTurn.delete(turnId);
        const parts = assistantPartsByTurn.get(turnId);
        if (parts) parts.push(message);
        else assistantPartsByTurn.set(turnId, [message]);
        const previous = assistantTextByTurn.get(turnId);
        assistantTextByTurn.set(turnId, previous ? `${previous}\n\n${message.text}` : message.text);
        return;
      }
      messageTurnIds.add(turnId);
      lastPromptIndexByTurn.set(turnId, index);
      promptMessageIndexes.push(index);
      if (message.role !== "user") return;
      const prompts = userPromptIndexesByTurn.get(turnId);
      if (prompts) prompts.push(index);
      else userPromptIndexesByTurn.set(turnId, [index]);
    });
    return {
      assistantMessagesByTurn, assistantPartsByTurn, assistantTextByTurn, assistantMessageIdByTurn,
      messageTurnIds, lastPromptIndexByTurn, promptMessageIndexes, userPromptIndexesByTurn,
    };
  }, [view.messages]);
  const turnStatusById = useMemo(
    () => new Map(view.turns.map((turn) => [turn.turn_id, turn.status])),
    [view.turns],
  );

  useEffect(() => {
    if (!jumpTarget?.messageId) return;
    for (const [turnId, segments] of Object.entries(turnFold.segmentsByTurn)) {
      const { activity } = presentConversationTurn(segments, turnStatusById.get(turnId) || "");
      if (!activity.some(segment => segment.kind === "text" && segment.spans?.some(span => span.messageId === jumpTarget.messageId))) continue;
      setExpandByTurn(current => current[turnId]?.processOpen ? current : {
        ...current, [turnId]: { ...current[turnId], processOpen: true },
      });
      break;
    }
  }, [jumpTarget, turnFold.segmentsByTurn, turnStatusById]);

  const isCredentialFailure = /credential|auth|token|凭据|认证/i.test(failureMessage);
  const failureKey = (isLastTurnFailed || isLastTurnInterrupted || isLastTurnCancelled)
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
        : isLastTurnCancelled
          ? "已取消"
          : isLastTurnInterrupted
          ? "已中断"
          : isLastTurnFailed
            ? "执行失败"
            : "执行已暂停"}
      detail={panelDetail}
      interrupted={!commandError && (isLastTurnInterrupted || isLastTurnCancelled)}
      credentialFailure={isCredentialFailure || /credential|auth|token|凭据|认证/i.test(commandError)}
      busy={busy}
      onContinue={showRuntimeFailure && !isLastTurnCancelled ? onContinueTurn : undefined}
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
  const activeTurnSegments = view.state.running_turn_id
    ? turnFold.segmentsByTurn[view.state.running_turn_id] || []
    : [];
  const liveStatus = running && connected && !pendingApproval && !pendingInput
    ? resolveConversationLiveActivity(activeTurnSegments).label
    : !running && lastTurn?.status === "completed"
      ? "本轮执行已完成"
      : "";
  const steeredReplySegments = useMemo(() => {
    const result = new Map<number, { segments: ConversationTurnSegment[]; text: string; before?: { segments: ConversationTurnSegment[]; text: string } }>();
    const steeredTurns = new Map([...userPromptIndexesByTurn].filter(([, indexes]) => (
      indexes.some(index => view.messages[index].kind === "steer")
    )));
    if (!steeredTurns.size) return result;
    const eventsByTurn = new Map<string, ConversationEvent[]>();
    for (const event of foldEvents) {
      const turnId = event.payload.turn_id;
      if (typeof turnId !== "string" || !steeredTurns.has(turnId)) continue;
      const turnEvents = eventsByTurn.get(turnId);
      if (turnEvents) turnEvents.push(event);
      else eventsByTurn.set(turnId, [event]);
    }
    for (const [turnId, indexes] of steeredTurns) {
      const buckets = indexes.map(() => [] as ConversationEvent[]);
      const toolOwners = new Map<string, number>();
      const prefixEvents: ConversationEvent[] = [];
      const assistantParts = assistantPartsByTurn.get(turnId) ?? [];
      const firstSteerSeq = view.messages[indexes[0]].kind === "steer"
        ? view.messages[indexes[0]].stream_seq ?? 0 : 0;
      for (const event of eventsByTurn.get(turnId) ?? []) {
        let owner = event.seq < firstSteerSeq ? -1 : 0;
        for (let i = 0; i < indexes.length; i += 1) {
          const message = view.messages[indexes[i]];
          if (message.kind === "steer" && (message.stream_seq ?? Infinity) <= event.seq) owner = i;
        }
        const toolId = String(event.payload.tool_call_id || event.payload.call_id || event.payload.invocation_id || "");
        if (toolId) {
          if (!toolOwners.has(toolId)) toolOwners.set(toolId, owner);
          owner = toolOwners.get(toolId)!;
        }
        if (owner < 0) prefixEvents.push(event);
        else buckets[owner].push(event);
      }
      indexes.forEach((index, i) => {
        const start = i === 0 ? firstSteerSeq : view.messages[index].stream_seq ?? Infinity;
        const end = view.messages[indexes[i + 1]]?.stream_seq ?? Infinity;
        const text = assistantParts.filter(message => (
          (message.stream_seq ?? 0) >= start && (message.stream_seq ?? 0) < end
        )).map(message => message.text).join("\n\n");
        const before = i === 0 && firstSteerSeq > 0 ? {
          segments: buildConversationTurnFold(prefixEvents).segmentsByTurn[turnId] || [],
          text: assistantParts.filter(message => (message.stream_seq ?? 0) < firstSteerSeq)
            .map(message => message.text).join("\n\n"),
        } : undefined;
        result.set(index, { segments: buildConversationTurnFold(buckets[i]).segmentsByTurn[turnId] || [], text, before });
      });
    }
    return result;
  }, [view.messages, foldEvents, assistantPartsByTurn, userPromptIndexesByTurn]);
  const runningTurnId = view.state.running_turn_id;
  const { timelineRows, leftoverTurnIdsBeforeMessage } = useMemo(() => {
    const turnIds = new Set<string>();
    for (const turn of view.turns) {
      if (
        assistantMessagesByTurn.has(turn.turn_id)
        || generatedImagesByTurn.has(turn.turn_id)
        || ["failed", "interrupted", "cancelled"].includes(turn.status)
      ) {
        turnIds.add(turn.turn_id);
      }
    }
    if (running && runningTurnId) turnIds.add(runningTurnId);
    for (const turnId of Object.keys(turnFold.segmentsByTurn)) turnIds.add(turnId);
    const leftoverTurnIds = [...turnIds].filter(turnId => (
      turnId && !messageTurnIds.has(turnId) && (turnStatusById.has(turnId) || turnId === runningTurnId)
    ));
    const leftoverTurnIdsBeforeMessage = new Map<number, string[]>();
    const trailingLeftoverTurnIds: string[] = [];
    if (leftoverTurnIds.length) {
      const unknownSequence = Number.MAX_SAFE_INTEGER;
      const sequenceByTurn = new Map(view.turns.map((turn, index) => [turn.turn_id, turn.seq ?? index + 1]));
      let greatestSequence = -Infinity;
      const promptBoundaries = promptMessageIndexes.map(index => {
        greatestSequence = Math.max(greatestSequence, sequenceByTurn.get(view.messages[index].turn_id || "") ?? unknownSequence);
        return greatestSequence;
      });
      for (const turnId of leftoverTurnIds) {
        const sequence = sequenceByTurn.get(turnId) ?? unknownSequence;
        let start = 0;
        let end = promptBoundaries.length;
        while (start < end) {
          const middle = start + Math.floor((end - start) / 2);
          if (promptBoundaries[middle] > sequence) end = middle;
          else start = middle + 1;
        }
        const nextPromptIndex = promptMessageIndexes[start];
        if (nextPromptIndex === undefined) {
          trailingLeftoverTurnIds.push(turnId);
        } else {
          const rows = leftoverTurnIdsBeforeMessage.get(nextPromptIndex);
          if (rows) rows.push(turnId);
          else leftoverTurnIdsBeforeMessage.set(nextPromptIndex, [turnId]);
        }
      }
    }
    const timelineRows = buildConversationTimelineRows({
      messages: view.messages,
      leftoverTurnIdsBeforeMessage,
      trailingLeftoverTurnIds,
      assistantMessageIdByTurn,
      lastPromptIndexByTurn,
    });
    return { timelineRows, leftoverTurnIdsBeforeMessage };
  }, [
    view.messages, view.turns, running, runningTurnId, turnFold.segmentsByTurn, turnStatusById,
    assistantMessagesByTurn, generatedImagesByTurn, assistantMessageIdByTurn, messageTurnIds, promptMessageIndexes, lastPromptIndexByTurn,
  ]);
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

  const resolveTurnRuntime = (turnId: string): ConversationTurnRuntime => {
    const recorded = runtimeByTurn[turnId];
    const snapshot = turnById.get(turnId)?.runtime_snapshot;
    if (recorded || snapshot) return {
      adapterId: recorded?.adapterId || snapshot?.adapter_id || "",
      instanceId: recorded?.instanceId || snapshot?.instance_id || "default",
      credentialId: recorded?.credentialId || snapshot?.credential_id || "",
      model: recorded?.model || snapshot?.model || "",
      effort: recorded?.effort || snapshot?.effort || "",
      accessMode: recorded?.accessMode || snapshot?.access_mode || "",
    };
    if (turnId && turnId === view.state.running_turn_id) return liveSelectorRuntime();
    return EMPTY_TURN_RUNTIME;
  };

  const identityKeyForTurn = (turnId: string): string => {
    const turnRuntime = resolveTurnRuntime(turnId);
    return runtimeIdentityKey(resolveAssistantRuntimeIdentity(turnRuntime, credentials));
  };

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
        if (assistantMsg || generatedImagesByTurn.has(turnId) || hasSegments || isRunningTurn || isSettledTurn) return turnId;
      }
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
    showPlanBadge = false,
    replyPart?: { segments: ConversationTurnSegment[]; latest: boolean },
  ) => {
    const turnStatus = turnStatusById.get(turnId) || (isStreaming ? "running" : "");
    const segments = settleTurnSegmentsAgainstStatus(
      replyPart?.segments ?? turnFold.segmentsByTurn[turnId] ?? [],
      turnStatus,
    );
    const isSettled = (replyPart?.latest ?? true) && ["completed", "failed", "interrupted", "aborted", "cancelled", "canceled"].includes(turnStatus);
    const usePersistedText = Boolean(fallbackText)
      && !segments.some((segment) => segment.kind === "text" && segment.text.trim());
    const sourceMessages = assistantPartsByTurn.get(turnId);
    const persistedSources = (text: string) => {
      const messages = sourceMessages ?? [];
      const matches = messages.filter(message => message.text === text);
      if (matches.length === 1) return [{ messageId: matches[0].message_id, start: 0, end: text.length }];
      if (messages.map(message => message.text).join("\n\n") !== text) return [];
      let offset = 0;
      return messages.map(message => {
        const span = { messageId: message.message_id, start: offset, end: offset + message.text.length };
        offset = span.end + 2;
        return span;
      });
    };
    const process = processByTurn[turnId];
    const streamingText = !replyPart && isStreaming && turnId === view.state.running_turn_id ? liveText : "";
    const display: ConversationTurnSegment[] = usePersistedText
      ? [
          ...segments,
          {
          kind: "text",
          turnId,
          text: fallbackText,
          phase: turnStatus === "completed" ? "final_answer" : "unknown",
          spans: persistedSources(fallbackText),
          },
        ]
      : replyPart ? segments : restoreConversationStreamPrefix(segments, streamingText, liveTextRuns, turnId);
    const presentation = presentConversationTurn(display, turnStatus);
    const answerSpans = presentation.answerSpans.length ? presentation.answerSpans : persistedSources(presentation.answerText);
    const turnRuntime = resolveTurnRuntime(turnId);
    const runtimeIdentity = resolveAssistantRuntimeIdentity(turnRuntime, credentials);
    const lastDisplay = display.at(-1);
    if (!display.length && !generatedImagesByTurn.has(turnId) && !isStreaming && !isSettled) return null;
    const answerSources = isSettled && !isStreaming
      ? [...new Set(extractUrls(presentation.answerText))].filter((url) => !isLocalPreviewUrl(url))
      : [];
    const turnDiffArtifact = isSettled ? diffArtifactByTurn.get(turnId) : undefined;
    const isLatestTurn = turnId === lastTurn?.turn_id;
    const completedAt = turnById.get(turnId)?.completed_at || timingByTurn[turnId]?.finishedAt;
    const agentInitiated = turnById.get(turnId)?.kind === "native_continuation";

    return (
      <div className="assistant flex w-full flex-col">
        {agentInitiated ? <AgentInitiatedTurnBadge turnId={turnId} /> : null}
        {showPlanBadge ? <PlanTurnBadge turnId={turnId} align="start" /> : null}
        <AssistantIdentity
          createdAt={createdAt || turnById.get(turnId)?.created_at}
          runtime={runtimeIdentity}
          collapsed={collapseIdentity && !agentInitiated}
        />
        {process && (isStreaming || expandByTurn[turnId]?.processOpen)
          && (process.loading || process.error || process.hasMore) ? (
          <div className="mt-2 flex flex-wrap items-center gap-2 text-cx-xs text-cx-fg-3" role="status">
            {process.loading ? <><Spinner size={12} />正在读取过程…</> : null}
            {process.error ? <span className="whitespace-pre-wrap text-cx-danger">{process.error}</span> : null}
            {process.hasMore ? <span>已读取 {hydratedEventsByTurn[turnId]?.length || 0} 条，过程尚未完整</span> : null}
            {!process.loading && (process.hasMore || process.error) ? (
              <Button size="xs" variant="ghost" onClick={() => void ensureTurnProcess(turnId, Boolean(process.nextAfterSeq || process.nextBeforeSeq))}>
                {process.error ? "重试读取过程" : process.nextBeforeSeq ? "加载更早过程" : "加载后续过程"}
              </Button>
            ) : null}
          </div>
        ) : null}
        <ConversationTurnProcess
          turnId={turnId}
          status={turnStatus}
          segments={presentation.activity}
          answerText={presentation.answerText}
          sourceMessages={sourceMessages}
          onCiteSelection={onCiteMessageSpan}
          onAskSideChat={onAskSideChat}
          running={isStreaming}
          liveIndicator={!pendingApproval && !pendingInput}
          processRecorded={Boolean(turnById.get(turnId)?.process_recorded || turnById.get(turnId)?.historical)}
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
          }}
          planSummary={(() => {
            const plan = view.state.plan;
            if (!plan?.tasks?.length) return "";
            if (plan.turn_id && plan.turn_id !== turnId) return "";
            if (!plan.turn_id && view.state.running_turn_id && view.state.running_turn_id !== turnId) {
              return "";
            }
            const done = plan.tasks.filter((task) => task.status === "completed").length;
            return `${done}/${plan.tasks.length} 步骤`;
          })()}
          onOpenDrawer={onOpenDrawer}
          threadId={threadId}
          agentRoots={replyPart && !replyPart.latest ? undefined : agentRootsByTurn.get(turnId)}
          agentTools={turnFold.agentToolsByTurn[turnId]}
          onOpenThread={onOpenThread}
          onCancelSubagent={handleCancelSubagent}
          onOpenToolDiff={(targetTurnId, filePath) => {
            if (threadId) chatPanel.openDiff(threadId, { kind: "turn", turnId: targetTurnId, filePath });
            else onOpenDiffBaseline?.({ kind: "turn", turnId: targetTurnId });
          }}
        />
        {(replyPart?.latest ?? true) && generatedImagesByTurn.get(turnId)?.map(image => (
          <div key={image.artifact_id} className="my-2">
            <ChatImage src={`artifact:${image.artifact_id}`} alt={image.name || "生成图片"} threadId={threadId} className="cx-md-img" />
          </div>
        ))}
        {presentation.answerText ? (
          <ConversationMessage
            role="assistant"
            messageId={replyPart && !replyPart.latest ? undefined : `msg-asst-${turnId}`}
            readingMessageId={replyPart && !replyPart.latest ? "" : assistantMessageIdByTurn.get(turnId) ?? ""}
            turnId={turnId}
            text={presentation.answerText}
            isStreaming={isStreaming && lastDisplay?.kind === "text"
              && lastDisplay.phase === "final_answer"}
            showIdentity={false}
            showActions={isSettled && !isStreaming}
            onRetry={onRetryTurn ? () => onRetryTurn(turnId) : undefined}
            onFork={onForkTurn ? () => onForkTurn(turnId) : undefined}
            onCiteSelection={onCiteMessageSpan}
            onAskSideChat={onAskSideChat}
            citationSources={answerSpans}
            sourceMessages={sourceMessages}
            onResourceLink={onResourceLink}
            highlightRange={jumpRange}
            annotations={annotations}
            onRewind={onRewindTurn ? () => onRewindTurn(turnId) : undefined}
            rewindDisabled={rewindDisabled}
            rewindTitle={rewindReason || "原生回退不可用"}
            actionsDisabled={busy || running}
            threadId={threadId}
            pinActions={isLatestTurn && !running}
            actionMeta={isSettled && completedAt ? <MessageCompletionTime completedAt={completedAt} /> : undefined}
            className="mt-1"
          />
        ) : null}
        {answerSources.length ? <SourceChips urls={answerSources} className="mt-2" /> : null}
        {turnStatus === "failed" && (conversationErrorMessage(turnById.get(turnId)?.error) || (isLatestTurn ? failureMessage : "")) ? (
          <TurnErrorCard
            message={conversationErrorMessage(turnById.get(turnId)?.error) || failureMessage}
            retryAt={retryByTurn[turnId]}
            onRetry={isLatestTurn && onContinueTurn
              ? onContinueTurn
              : onRetryTurn
                ? () => onRetryTurn(turnId)
                : undefined}
          />
        ) : null}
        {turnAccessory && isSettled && !isStreaming ? turnAccessory(turnId) : null}
        {turnDiffArtifact && threadId ? (
          <ChangedFilesCard
            threadId={threadId}
            turnId={turnId}
            artifact={turnDiffArtifact}
            partial={changesByTurn[turnId]?.completeness === "partial"}
            onOpenDiff={(request) => onOpenDiffBaseline?.({
              kind: "turn",
              turnId: request.turnId,
              artifactId: request.artifactId,
            })}
            className="mt-3"
          />
        ) : null}
      </div>
    );
  };

  const estimatedRowHeights = useMemo(() => (
    timelineRows.map((row) => {
      if (row.kind === "orphan-turn") {
        const asstText = assistantTextByTurn.get(row.turnId) || "";
        const height = estimateAssistantMarkdownHeight(asstText)
          + (generatedImagesByTurn.has(row.turnId) ? 280 : 0)
          + 28;
        return Math.max(120, Math.round(height));
      }
      const msg = view.messages[row.messageIndex];
      let height = 28;
      for (const orphanTurnId of row.orphanTurnIds) {
        height += estimateAssistantMarkdownHeight(assistantTextByTurn.get(orphanTurnId) || "")
          + (generatedImagesByTurn.has(orphanTurnId) ? 280 : 0)
          + 28;
      }
      if (!msg) return Math.max(120, Math.round(height));
      if (msg.role === "user") {
        height += estimateUserBubbleHeight(msg.text);
        const turnId = msg.turn_id || "";
        const asstText = turnId ? (assistantTextByTurn.get(turnId) || "") : "";
        if (asstText) height += estimateAssistantMarkdownHeight(asstText);
        if (turnId && generatedImagesByTurn.has(turnId)) height += 280;
      } else {
        height += estimateAssistantMarkdownHeight(msg.text);
      }
      return Math.max(120, Math.round(height));
    })
  ), [assistantTextByTurn, generatedImagesByTurn, timelineRows, view.messages]);
  const estimatedRowHeightsRef = useRef<number[]>([]);
  estimatedRowHeightsRef.current = estimatedRowHeights;

  const getTimelineRowKey = useCallback((index: number) => timelineRows[index]?.key || index, [timelineRows]);
  const estimateTimelineRowSize = useCallback(
    (index: number) => estimatedRowHeightsRef.current[index] ?? 180,
    [],
  );
  const extractTimelineRange = useCallback(
    (range: { startIndex: number; endIndex: number; overscan: number; count: number }) => {
      if (range.count <= 0 || range.endIndex < range.startIndex) return [];
      const maxSteps = Math.max(1, range.overscan);
      const sizeOf = (index: number) => estimatedRowHeightsRef.current[index] ?? 180;
      let start = Math.max(0, Math.min(range.count - 1, range.startIndex));
      let abovePx = 0;
      for (let step = 0; step < maxSteps && start > 0; step += 1) {
        const nextSize = sizeOf(start - 1);
        if (step > 0 && nextSize >= HISTORY_OVERSCAN_HEAVY_ROW_PX) break;
        start -= 1;
        abovePx += nextSize;
        if (abovePx >= HISTORY_OVERSCAN_BUDGET_PX) break;
      }
      let end = Math.max(start, Math.min(range.count - 1, range.endIndex));
      let belowPx = 0;
      for (let step = 0; step < maxSteps && end < range.count - 1; step += 1) {
        const nextSize = sizeOf(end + 1);
        if (step > 0 && nextSize >= HISTORY_OVERSCAN_HEAVY_ROW_PX) break;
        end += 1;
        belowPx += nextSize;
        if (belowPx >= HISTORY_OVERSCAN_BUDGET_PX) break;
      }
      const indexes: number[] = [];
      for (let i = start; i <= end; i += 1) indexes.push(i);
      return indexes;
    },
    [],
  );
  const rowVirtualizer = useVirtualizer({
    useAnimationFrameWithResizeObserver: true,
    count: timelineRows.length,
    getScrollElement: () => streamRef.current,
    estimateSize: estimateTimelineRowSize,
    anchorTo: "end",
    scrollEndThreshold: -1,
    overscan: 3,
    rangeExtractor: extractTimelineRange,
    getItemKey: getTimelineRowKey,
    directDomUpdates: true,
    directDomUpdatesMode: "position",
  });
  const virtualRows = rowVirtualizer.getVirtualItems();
  const visibleTurnIds = useMemo(() => {
    const ids = new Set<string>();
    for (const item of virtualRows) {
      const row = timelineRows[item.index];
      if (!row) continue;
      if (row.turnId) ids.add(row.turnId);
      if (row.kind === "message") for (const turnId of row.orphanTurnIds) ids.add(turnId);
    }
    return ids;
  }, [timelineRows, virtualRows]);
  const steeredTurnIds = useMemo(() => new Set(view.messages.filter(message => message.kind === "steer").map(message => message.turn_id)), [view.messages]);
  const neededProcessTurns = useMemo(() => {
    const ids = new Set<string>();
    for (const turnId of visibleTurnIds) {
      if (expandByTurn[turnId]?.processOpen || steeredTurnIds.has(turnId)) ids.add(turnId);
    }
    if (resumedTurnId && resumedTurnId === runningTurnId) ids.add(resumedTurnId);
    return ids;
  }, [expandByTurn, resumedTurnId, runningTurnId, steeredTurnIds, visibleTurnIds]);
  useEffect(() => {
    for (const [turnId, request] of processRequestsRef.current) {
      if (neededProcessTurns.has(turnId)) continue;
      request.abort();
      processRequestsRef.current.delete(turnId);
    }
    const unused = Object.keys(processByTurn).filter(turnId => !neededProcessTurns.has(turnId));
    if (unused.length) {
      setHydratedEventsByTurn((current) => {
        const next = { ...current };
        for (const turnId of unused) delete next[turnId];
        return next;
      });
      setProcessByTurn((current) => {
        const next = { ...current };
        for (const turnId of unused) delete next[turnId];
        return next;
      });
    }
    for (const turnId of neededProcessTurns) {
      if (!processByTurn[turnId]) void ensureTurnProcess(turnId);
    }
  }, [ensureTurnProcess, neededProcessTurns, processByTurn]);
  isVirtualizerScrollingRef.current = () => rowVirtualizer.isScrolling;
  rowVirtualizerRef.current = rowVirtualizer;
  const { containerRef: virtualContainerRef } = rowVirtualizer;
  const setVirtualListNode = useCallback((node: HTMLDivElement | null) => {
    virtualListRef.current = node;
    virtualContainerRef(node);
  }, [virtualContainerRef]);
  const readRowMeasurements = useCallback(() => {
    rowVirtualizer.getTotalSize();
    return rowVirtualizer.measurementsCache;
  }, [rowVirtualizer]);

  useLayoutEffect(() => {
    const listNode = virtualListRef.current;
    const element = streamRef.current;
    const threadKey = view.thread.thread_id;
    const listTop = listNode ? listNode.offsetTop : null;
    const prevTop = prevVirtualListTopRef.current?.threadId === threadKey ? prevVirtualListTopRef.current.top : null;
    if (listTop !== null) prevVirtualListTopRef.current = { threadId: threadKey, top: listTop };
    if (
      element
      && listTop !== null
      && prevTop !== null
      && listTop !== prevTop
      && !stickToBottomRef.current
      && !restoringAnchorRef.current
      && !String(highlightedMessageId || "").trim()
    ) {
      element.scrollTop += listTop - prevTop;
    }
  }, [
    highlightedMessageId,
    loadingOlder,
    showSuperseded,
    view.messages_page?.has_more_before,
    view.thread.thread_id,
    view.lineage,
    view.state?.lineage,
  ]);

  useLayoutEffect(() => {
    const pinned = prependAnchorRef.current;
    if (!pinned) return;
    if (!pinned.settling) {
      if (!pendingAnchorRef.current || pinned.threadId !== view.thread.thread_id) {
        releasePrependAnchor();
        return;
      }
      if (view.messages[0]?.message_id === pinned.firstMessageId) return;
      pinned.settling = true;
      const list = virtualListRef.current;
      if (list) {
        prependObserverRef.current?.observe(list);
        for (const row of list.children) {
          const index = row instanceof HTMLElement ? Number(row.dataset.index) : Number.NaN;
          if (Number.isInteger(index)) rowVirtualizer.resizeItem(index, rowVirtualizer.options.measureElement(row, undefined, rowVirtualizer));
        }
      }
    }
    holdPrependAnchor();
  });

  useLayoutEffect(() => {
    const anchor = pendingAnchorRef.current;
    const element = streamRef.current;
    if (!anchor || !element || !anchor.succeeded || anchor.threadId !== view.thread.thread_id) return;
    if (anchor.firstMessageId === view.messages[0]?.message_id) {
      pendingAnchorRef.current = null;
      return;
    }
    pendingAnchorRef.current = null;
    const frame = requestAnimationFrame(() => {
      persistCurrentAnchor(element, false);
    });
    return () => cancelAnimationFrame(frame);
  }, [anchorCommit, loadingOlder, persistCurrentAnchor, view.messages, view.thread.thread_id]);

  const jumpKey = jumpTarget?.key || "";
  const jumpRange = useMemo(() => (jumpTarget ? {
    key: jumpTarget.key,
    messageId: jumpTarget.messageId,
    startOffset: jumpTarget.startOffset || 0,
    endOffset: jumpTarget.endOffset || 0,
  } : null), [jumpTarget]);
  const jumpTargetRef = useRef(jumpTarget);
  jumpTargetRef.current = jumpTarget;
  const onJumpTargetConsumedRef = useRef(onJumpTargetConsumed);
  onJumpTargetConsumedRef.current = onJumpTargetConsumed;
  const messagesRef = useRef(view.messages);
  messagesRef.current = view.messages;
  useEffect(() => {
    const target = jumpTargetRef.current;
    if (!jumpKey || !target?.messageId) return;
    stickToBottomRef.current = false;
    setShowJumpToLatest(true);
    const handled = () => Boolean(streamRef.current?.querySelector(`[data-cite-jump="${CSS.escape(jumpKey)}"]`));
    if (!handled()) {
      const rows = timelineRowsRef.current;
      let index = timelineRowIndexForMessageId(rows, target.messageId);
      const turnId = target.turnId || messagesRef.current.find((message) => message.message_id === target.messageId)?.turn_id || "";
      if (index < 0 && turnId) {
        index = rows.findIndex((row) => row.kind === "message"
          ? row.turnId === turnId || row.orphanTurnIds.includes(turnId)
          : row.turnId === turnId);
      }
      if (index >= 0) rowVirtualizer.scrollToIndex(index, { align: "start", behavior: "auto" });
    }
    let frame = 0, tries = 0, flashed: HTMLElement | null = null;
    const fallback = () => {
      if (handled()) return;
      const element = streamRef.current?.querySelector<HTMLElement>(
        `[data-message-id="${CSS.escape(target.messageId)}"], [data-citation-message-ids~="${CSS.escape(target.messageId)}"]`,
      );
      if (element && tries >= 6) {
        element.scrollIntoView({ block: "start", behavior: "auto" });
        element.classList.add("is-cite-highlight");
        flashed = element;
        return;
      }
      if (++tries < 30) frame = requestAnimationFrame(fallback);
    };
    frame = requestAnimationFrame(fallback);
    const timer = window.setTimeout(() => onJumpTargetConsumedRef.current?.(), 2600);
    return () => {
      cancelAnimationFrame(frame);
      window.clearTimeout(timer);
      flashed?.classList.remove("is-cite-highlight");
    };
  }, [jumpKey, rowVirtualizer]);

  useEffect(() => {
    rowVirtualizer.shouldAdjustScrollPositionOnItemSizeChange = (item, _delta, instance) => {
      if (stickToBottomRef.current || prependAnchorRef.current?.settling) return false;
      const offset = (instance.scrollOffset ?? 0) + instance.scrollAdjustments;
      const size = instance.itemSizeCache.get(item.key);
      return size === undefined ? item.start < offset : item.start + size <= offset;
    };
  }, [rowVirtualizer]);

  useEffect(() => {
    const target = String(highlightedMessageId || "").trim();
    if (!target) {
      highlightScrollDoneRef.current = "";
      return;
    }
    scrollRequestRef.current += 1;
    pendingAnchorRef.current = null;
    if (showSuperseded && supersededMessages.some(message => message.message_id === target)) {
      stickToBottomRef.current = false;
      setShowJumpToLatest(true);
      const frame = requestAnimationFrame(() => {
        streamRef.current?.querySelector<HTMLElement>(`[data-audit-message-id="${CSS.escape(target)}"]`)?.scrollIntoView({ block: "center", behavior: "auto" });
      });
      return () => cancelAnimationFrame(frame);
    }
    const index = timelineRowIndexForMessageId(timelineRows, target);
    if (index < 0) {
      highlightScrollDoneRef.current = "";
      return;
    }
    stickToBottomRef.current = false;
    setShowJumpToLatest(true);

    const scrollToTarget = () => {
      rowVirtualizer.scrollToIndex(index, { align: "center", behavior: "auto" });
    };

    scrollToTarget();
    const frame = window.requestAnimationFrame(scrollToTarget);
    const retry = window.setTimeout(() => {
      scrollToTarget();
      const el = streamRef.current?.querySelector(`[data-message-id="${CSS.escape(target)}"]`);
      if (el) {
        el.scrollIntoView({ block: "center", behavior: "auto" });
        highlightScrollDoneRef.current = target;
      }
    }, 80);
    const retry2 = window.setTimeout(() => {
      const el = streamRef.current?.querySelector(`[data-message-id="${CSS.escape(target)}"]`);
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
  }, [highlightedMessageId, rowVirtualizer, timelineRows, view.messages, showSuperseded, supersededMessages]);

  const scrollToMessageAnchor = useCallback(async (
    messageId: string,
    offsetPx = 0,
    options?: { highlight?: boolean; elementMessageId?: string },
  ): Promise<boolean> => {
    const target = String(messageId || "").trim();
    if (!target) return false;
    const anchor: ReadingAnchor = { messageId: target, offsetPx, elementMessageId: options?.elementMessageId };
    const request = ++anchorRequestRef.current;
    const isCurrent = () => anchorRequestRef.current === request;
    if (anchorPersistTimerRef.current) {
      clearTimeout(anchorPersistTimerRef.current);
      anchorPersistTimerRef.current = null;
    }
    restoringAnchorRef.current = true;
    releasePrependAnchor();
    stickToBottomRef.current = false;
    setShowJumpToLatest(true);
    lastAnchorRef.current = anchor;
    try {
      if (onEnsureMessageVisible) {
        const result = await onEnsureMessageVisible(target);
        if (result !== "ok" || !isCurrent()) return false;
      }
      const ok = await restoreMeasuredReadingAnchor({
        stream: () => streamRef.current,
        ...anchor,
        isCurrent,
        reveal: () => {
          const index = timelineRowIndexForMessageId(timelineRowsRef.current, target);
          const element = streamRef.current;
          const offset = index >= 0 ? rowVirtualizer.getOffsetForIndex(index, "start") : undefined;
          if (element && offset && element.scrollTop !== offset[0]) element.scrollTop = offset[0];
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
      lastAnchorRef.current = anchor;
      restoreDoneRef.current = view.thread.thread_id;
      writeReadingPosition({
        threadId: view.thread.thread_id,
        ...anchor,
        stickToBottom: false,
        expandByTurn: expandByTurnRef.current,
      });
      return true;
    } finally {
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
      if (isCurrent()) {
        restoringAnchorRef.current = false;
        const element = streamRef.current;
        if (element) updateReaderPosition(element.scrollHeight - element.scrollTop - element.clientHeight);
      }
    }
  }, [onEnsureMessageVisible, releasePrependAnchor, rowVirtualizer, updateReaderPosition, view.thread.thread_id]);

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
      const ok = await scrollToMessageAnchor(saved.messageId, saved.offsetPx, { elementMessageId: saved.elementMessageId });
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
    void scrollToMessageAnchor(entry.messageId, entry.offsetPx, { highlight: true, elementMessageId: entry.elementMessageId });
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

  const minimapMarkers = useMemo(() => {
    const rowMeasurements = readRowMeasurements();
    const markers: MinimapMarker[] = [];
    timelineRows.forEach((row, index) => {
      if (row.kind !== "message" || row.role !== "user") return;
      const msg = view.messages[row.messageIndex];
      if (!msg || msg.kind === "steer") return;
      const status = String(turnStatusById.get(row.turnId) || "").toLowerCase();
      markers.push({
        key: row.key,
        messageId: row.messageId,
        index,
        start: rowMeasurements[index]?.start ?? 0,
        label: msg.text.replace(/\s+/g, " ").trim(),
        preview: replyExcerpt(assistantTextByTurn.get(row.turnId) || ""),
        tone: running && row.turnId && row.turnId === runningTurnId
          ? "running"
          : status === "failed"
            ? "danger"
            : ["interrupted", "aborted", "cancelled", "canceled"].includes(status) ? "muted" : "default",
      });
    });
    return markers;
  }, [assistantTextByTurn, readRowMeasurements, running, runningTurnId, timelineRows, turnStatusById, view.messages]);

  const turnNavAvailable = useMemo(() => userTurnMessageIds(view.messages).length > 1, [view.messages]);
  const showReaderOverlay = showJumpToLatest || Boolean(view.messages_page?.has_more_after);

  const lineage = view.lineage ?? view.state?.lineage ?? null;
  const lineageParentTitle = (view.lineage && view.lineage.parent_title) || "父对话";

  const handleCancelSubagent = useCallback(async (subagentId: string) => {
    await sendConversationCommand(view.thread.thread_id, "conversation.subagent.cancel", {
      subagent_id: subagentId,
      reason: "user_cancelled",
    });
    toast({ title: "已请求取消委派线程", tone: "neutral", duration: 2400 });
  }, [view.thread.thread_id]);

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
          showJumpToLatest={showReaderOverlay}
          running={running}
          attentionLabel={attentionLabel}
          onPrev={() => void goToNeighborTurn(-1)}
          onNext={() => void goToNeighborTurn(1)}
          onJumpBack={returnToPreviousReading}
          onJumpToLatest={() => {
            jumpToLatest();
            if (!pendingApprovalTotal) return;
            window.setTimeout(() => {
              streamRef.current
                ?.querySelector<HTMLElement>('[data-approval-keyboard="true"]')
                ?.focus({ preventScroll: true });
            }, 260);
          }}
        />
      ) : null}
      streamAside={contentLoading || contentError ? null : (
        <>
          <ConversationMinimap
            streamRef={streamRef}
            listRef={virtualListRef}
            markers={minimapMarkers}
            readRowMeasurements={readRowMeasurements}
            onJump={(messageId) => void scrollToMessageAnchor(messageId, 0, { highlight: true })}
          />
          <ConversationSummaryCard
            threadId={view.thread.thread_id}
            agents={agentTree.agents}
            sources={conversationSources}
          />
        </>
      )}
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
          <p className="text-cx-sm leading-6 text-cx-fg-2">{contentError}</p>
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
        {lineage ? (
          <div
            className="-mb-3 flex min-w-0 items-center gap-1.5 rounded-lg border border-cx-border bg-cx-bg-subtle px-2.5 py-1.5 text-cx-xs text-cx-fg-3"
            data-testid="subagent-lineage-banner"
            data-lineage-depth={lineage.depth}
          >
            <Icon name="bot" size={13} className="shrink-0 text-cx-fg-4" />
            <span className="min-w-0 truncate">
              由 <span className="font-medium text-cx-fg-2">{lineageParentTitle}</span> 派生 · 深度 {lineage.depth}
            </span>
            {onOpenThread ? (
              <button
                type="button"
                className="ml-auto shrink-0 rounded px-1.5 py-0.5 text-cx-accent hover:bg-cx-hover"
                onClick={() => onOpenThread(lineage.parent_thread_id)}
              >
                查看父对话
              </button>
            ) : null}
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
            <h3 className="mb-3 flex items-center gap-1.5 text-cx-xs font-medium text-cx-fg-3">
              <Icon name="history" size={13} />
              已替代历史（仅供审计）
            </h3>
            <div className="flex flex-col gap-3">
              {supersededMessages.map((msg) => (
                <div key={`sup-${msg.message_id}`} data-audit-message-id={msg.message_id} className={highlightedMessageId === msg.message_id ? "conversation-message-highlight" : undefined}>
                <ConversationMessage
                  role={msg.role}
                  sourceProvider={msg.source_provider}
                  sourceContent={msg.source_content}
                  text={msg.text}
                  superseded
                  showActions={false}
                />
                </div>
              ))}
              {!supersededMessages.length && supersededTurns.map((turn) => (
                <div key={turn.turn_id} className="font-cx-mono text-cx-xs text-cx-fg-3">
                  #{turn.seq} · {turn.status} · {(turn.text || "").slice(0, 120)}
                </div>
              ))}
            </div>
          </section>
        ) : null}
        {(view.messages_page?.has_more_before || loadingOlder) ? (
          <div className="flex items-center justify-center gap-2 py-1 text-cx-xs text-cx-fg-4" aria-live="polite">
            {loadingOlder ? <Spinner size={12} /> : <Icon name="arrowUp" size={12} />}
            {loadingOlder ? "正在加载更早的消息…" : "向上滚动加载更早的消息"}
          </div>
        ) : null}
        <div
          ref={setVirtualListNode}
          className="relative w-full"
          data-virtualized-timeline="true"
          data-message-count={view.messages.length}
          data-row-count={timelineRows.length}
        >
          {virtualRows.map((virtualRow) => {
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
                >
                  {renderAssistantTurn(
                    turnId,
                    assistantTextByTurn.get(turnId)
                      || (running && turnId === view.state.running_turn_id ? liveText : ""),
                    assistantMsg?.created_at,
                    Boolean(running && turnId === view.state.running_turn_id),
                    collapseIdentity,
                    turnIsPlan(turnById.get(turnId)),
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
                  assistantTextByTurn.get(turnId)
                    || (running && turnId === view.state.running_turn_id ? liveText : ""),
                  assistantMessagesByTurn.get(turnId)?.created_at,
                  Boolean(running && turnId === view.state.running_turn_id),
                  collapseIdentity,
                  turnIsPlan(turnById.get(turnId)),
                )}
              </React.Fragment>
              );
            });
            let body: React.ReactNode = null;
            if (msg.role === "assistant") {
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
                    onAskSideChat={onAskSideChat}
                    onResourceLink={onResourceLink}
                    highlightRange={jumpRange}
                    annotations={annotations}
                  />
                </>
              );
            } else if (msg.role !== "user") {
              body = <>{orphanRows}<ConversationMessage role={msg.source_role || msg.role} sourceProvider={msg.source_provider} sourceContent={msg.source_content} messageId={msg.message_id} turnId={msg.turn_id || undefined} text={msg.text} createdAt={msg.created_at} showActions={false} /></>;
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
              const isSteer = msg.kind === "steer";
              const replyPart = steeredReplySegments.get(idx);
              const processRecorded = turnById.get(turnId)?.process_recorded;
              const assistantTurn = (isLastPromptForTurn || replyPart?.segments.length || replyPart?.text) && (assistantMsg || generatedImagesByTurn.has(turnId) || hasSegments || isRunningTurn || isSettledTurn || processRecorded)
                ? renderAssistantTurn(
                  turnId,
                  replyPart ? replyPart.text : assistantTextByTurn.get(turnId) || ((isRunningTurn || isSettledTurn) ? liveText : ""),
                  assistantMsg?.created_at,
                  isRunningTurn && isLastPromptForTurn,
                  (() => {
                    const prevLeftoverId = orphanTurns.at(-1);
                    if (prevLeftoverId) {
                      const prevKey = identityKeyForTurn(prevLeftoverId);
                      const nextKey = identityKeyForTurn(turnId);
                      return Boolean(prevKey && nextKey && prevKey === nextKey);
                    }
                    return shouldCollapseIdentity(turnId, idx);
                  })(),
                  false,
                  replyPart ? { segments: replyPart.segments, latest: isLastPromptForTurn } : undefined,
                )
                : null;
              body = (
                <>
                  {orphanRows}
                  {replyPart?.before ? renderAssistantTurn(turnId, replyPart.before.text, undefined, false, false, false,
                    { segments: replyPart.before.segments, latest: false }) : null}
                  <div className="flex w-full flex-col">
                  {!isSteer && turnIsPlan(turnById.get(turnId)) ? <PlanTurnBadge turnId={turnId} align="end" /> : null}
                  <ConversationMessage
                    role={msg.role}
                    messageId={msg.message_id}
                    turnId={msg.turn_id || undefined}
                    label={isSteer ? "引导" : undefined}
                    text={msg.text}
                    createdAt={msg.created_at}
                    attachments={resolveAttachments(isSteer ? msg.attachments || [] : turnById.get(turnId)?.attachments || [])}
                    threadId={threadId || undefined}
                    onOpenAttachment={onOpenAttachment}
                    contextRefs={
                      isSteer ? msg.capability_refs || [] : turnById.get(turnId)?.capability_refs || []
                    }
                    onJumpToContext={onJumpToContext}
                    onShowSentContext={
                      threadId && turnId && !isSteer && (turnById.get(turnId)?.has_context_delivery || turnById.get(turnId)?.context_delivery?.length)
                        ? () => setSentContextTurnId(turnId)
                        : undefined
                    }
                    className={isSteer ? "conversation-steer-message" : ""}
                    onEdit={
                      onEditTurn && turnId && !isSteer
                        ? () => onEditTurn(turnId, msg.text)
                        : undefined
                    }
                    onFork={
                      onForkTurn && turnId && !isSteer
                        ? () => onForkTurn(turnId)
                        : undefined
                    }
                    onRewind={
                      onRewindTurn && turnId && !isSteer
                        ? () => onRewindTurn(turnId)
                        : undefined
                    }
                    rewindDisabled={rewindDisabled}
                    rewindTitle={rewindReason || "原生回退不可用"}
                    actionsDisabled={busy || running}
                  />
                  </div>
                  {assistantTurn}
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
              >
                {body}
              </div>
            );
          })}
        </div>

        {threadId ? (
          <TurnContextDialog threadId={threadId} turnId={sentContextTurnId} onClose={() => setSentContextTurnId(null)} />
        ) : null}

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
  attentionLabel,
  onPrev,
  onNext,
  onJumpBack,
  onJumpToLatest,
}: {
  turnNavAvailable: boolean;
  hasJumpBack: boolean;
  showJumpToLatest: boolean;
  running: boolean;
  attentionLabel: string;
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
            initial={reduced ? { opacity: 0 } : { opacity: 0, transform: "translateY(8px) scale(0.96)" }}
            animate={reduced
              ? { opacity: 1, transition: { duration: 0.14, ease: EASE_OUT } }
              : { opacity: 1, transform: "translateY(0px) scale(1)", transition: { duration: 0.2, ease: EASE_OUT } }}
            exit={reduced
              ? { opacity: 0, transition: { duration: 0.1 } }
              : { opacity: 0, transform: "translateY(6px)", transition: { duration: 0.12, ease: EASE_OUT } }}
            className={pill}
          >
            <button
              type="button"
              onClick={onJumpToLatest}
              className={cn(
                "cx-press relative inline-flex h-7 items-center gap-1.5 rounded-full px-3 text-cx-sm font-medium hover:bg-cx-hover",
                attentionLabel ? "text-cx-warning" : "text-cx-fg-2 hover:text-cx-fg",
              )}
              data-testid="c08-jump-latest"
            >
              <Icon name={attentionLabel ? "shieldAlert" : "arrowDown"} size={13} />
              {attentionLabel || "回到最新"}
              {running && !attentionLabel ? <span className="size-1.5 rounded-full bg-cx-accent cx-pulse-dot" aria-hidden /> : null}
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
