"use client";

import React, { useCallback, useEffect, useId, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import {
  displayOffsetsForLocator,
  messageOffsetsToRange,
  messageRangeOffsets,
  sourceCodePointOffsets,
} from "@/lib/messageSelectionOffsets";
import { setTextHighlight } from "@/lib/textHighlights";
import { useLang } from "@/lib/i18n";
import { StreamingText } from "./StreamingMessage";
import type { VisualizationSources } from "@/components/chat/markdown/ChatMarkdown";
import { Icon, type IconName } from "@muteki/ui";
import { ContextNodeChip } from "./ComposerPromptDocument";
import { excerptPreview, normalizeContextNode, type ComposerContextNode } from "@/lib/composerContextDoc";
import type { ResourceLinkTarget } from "@/lib/resourcePreview";
import { MessageImages, isImageAttachment } from "@/components/chat/media/MessageImages";
import { IconButton, Menu, MenuItem, MenuSeparator, Popover, useContextMenu, useCopy } from "@muteki/ui";
import type { ConversationTextSpan } from "./conversationEventViews";
import {
  Message,
  MessageBubble,
  MessageBubbleCollapsible,
  MessageBubbleContent,
  MessageContent,
} from "@muteki/ui/agent";
import {
  CopyMessageButton,
  MessageActionBar,
  MessageActionButton,
  formatMessageTime,
} from "@/components/chat/timeline/MessageActions";

export interface MessageAttachmentChip {
  artifactId?: string;
  needsReselect?: boolean;
  name: string;
  size?: number;
  mediaType?: string;
  kind?: string;
}

export interface MessageAnnotationMark {
  id: string;
  index: number;
  messageId: string;
  startOffset: number;
  endOffset: number;
}

export interface MessageJumpRange {
  key: string;
  messageId: string;
  startOffset: number;
  endOffset: number;
}

export interface ConversationMessageProps {
  role: string;
  sourceProvider?: string;
  sourceContent?: unknown;
  text: string;
  messageId?: string;
  readingMessageId?: string;
  turnId?: string;
  label?: string;
  isStreaming?: boolean;
  createdAt?: string;
  attachments?: MessageAttachmentChip[];
  contextRefs?: Array<Record<string, unknown> | ComposerContextNode>;
  onCopy?: () => void;
  onRetry?: () => void;
  onFork?: () => void;
  onEdit?: () => void;
  onShowSentContext?: () => void;
  onRewind?: () => void;
  rewindDisabled?: boolean;
  rewindTitle?: string;
  onCiteSelection?: (payload: {
    messageId: string;
    turnId?: string;
    text: string;
    startOffset: number;
    endOffset: number;
    streaming?: boolean;
    comment?: string;
  }) => void;
  onAskSideChat?: ConversationMessageProps["onCiteSelection"];
  citationSources?: ConversationTextSpan[];
  sourceMessages?: VisualizationSources["messages"];
  onJumpToContext?: (payload: {
    messageId: string;
    turnId?: string;
    startOffset?: number;
    endOffset?: number;
  }) => void;
  onOpenAttachment?: (attachment: MessageAttachmentChip) => void;
  onResourceLink?: (target: ResourceLinkTarget) => void;
  highlightRange?: MessageJumpRange | null;
  annotations?: MessageAnnotationMark[];
  showIdentity?: boolean;
  showActions?: boolean;
  actionsDisabled?: boolean;
  className?: string;
  superseded?: boolean;
  threadId?: string;
  pinActions?: boolean;
  actionMeta?: React.ReactNode;
}

const USER_COLLAPSE_LINES = 6;

function attachmentIcon(attachment: MessageAttachmentChip): IconName {
  const type = `${attachment.mediaType || ""} ${attachment.name}`.toLowerCase();
  if (/image\/|\.(png|jpe?g|gif|webp|svg)$/.test(type)) return "image";
  if (/pdf/.test(type)) return "file";
  if (/\.(ts|tsx|js|py|go|rs|java|c|cpp|json|ya?ml|sh)$/.test(type)) return "fileCode";
  return "paperclip";
}

function formatSize(size?: number): string {
  if (!size) return "";
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(size < 10240 ? 1 : 0)} KB`;
  return `${(size / 1024 / 1024).toFixed(1)} MB`;
}

function AttachmentChip({ attachment, onOpen }: { attachment: MessageAttachmentChip; onOpen?: () => void }) {
  const size = formatSize(attachment.size);
  const content = (
    <>
      <span className="grid size-7 shrink-0 place-items-center rounded-lg bg-cx-hover text-cx-fg-3">
        <Icon name={attachmentIcon(attachment)} size={14} />
      </span>
      <span className="flex min-w-0 flex-col text-left">
        <span className="max-w-[180px] truncate text-cx-sm font-medium leading-4 text-cx-fg">{attachment.name}</span>
        {attachment.needsReselect
          ? <span className="text-cx-xs leading-4 text-cx-fg-4">旧版附件需重选原文件</span>
          : size ? <span className="text-cx-xs leading-4 text-cx-fg-4">{size}</span> : null}
      </span>
    </>
  );
  const className = "inline-flex max-w-full items-center gap-2 rounded-xl border border-cx-border-subtle bg-cx-elevated py-1 pl-1 pr-3";
  if (onOpen) {
    return (
      <button type="button" data-testid="message-attachment-chip" onClick={onOpen} className={cn(className, "cx-press hover:border-cx-border-strong")}>
        {content}
      </button>
    );
  }
  return <span data-testid="message-attachment-chip" className={className}>{content}</span>;
}

const USER_COLLAPSE_THRESHOLD_LINES = USER_COLLAPSE_LINES + 2;
const USER_DEFINITE_OVERFLOW_CHARS = 1600;
const USER_DEFINITE_FIT_CHARS = 80;
const USER_COLLAPSED_PREVIEW_MAX_CHARS = 2400;
const USER_COLLAPSED_PREVIEW_MIN_LINES = USER_COLLAPSE_LINES * 2;

function definitelyOverflowsUserBubble(text: string): boolean {
  if (text.length > USER_DEFINITE_OVERFLOW_CHARS) return true;
  let lines = 0;
  let pos = 0;
  const limit = text.endsWith("\n") ? USER_COLLAPSE_THRESHOLD_LINES + 1 : USER_COLLAPSE_THRESHOLD_LINES;
  while ((pos = text.indexOf("\n", pos)) !== -1) {
    lines += 1;
    if (lines >= limit) return true;
    pos += 1;
  }
  return false;
}

function definitelyFitsUserBubble(text: string): boolean {
  return text.length <= USER_DEFINITE_FIT_CHARS && !text.includes("\n");
}

function collapsedUserPreview(text: string): string {
  if (text.length <= USER_COLLAPSED_PREVIEW_MAX_CHARS) return text;
  let lines = 0;
  let pos = 0;
  while (pos < USER_COLLAPSED_PREVIEW_MAX_CHARS) {
    const next = text.indexOf("\n", pos);
    if (next === -1 || next >= USER_COLLAPSED_PREVIEW_MAX_CHARS) break;
    lines += 1;
    if (lines >= USER_COLLAPSED_PREVIEW_MIN_LINES) {
      return text.slice(0, next);
    }
    pos = next + 1;
  }
  let cut = USER_COLLAPSED_PREVIEW_MAX_CHARS;
  const code = text.charCodeAt(cut - 1);
  if (code >= 0xd800 && code <= 0xdbff) cut -= 1;
  return text.slice(0, cut);
}

function UserBubbleText({ text }: { text: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [state, setState] = useState(() => ({
    text,
    overflowing: definitelyOverflowsUserBubble(text),
    expanded: false,
  }));
  const current = state.text === text
    ? state
    : { text, overflowing: definitelyOverflowsUserBubble(text), expanded: false };
  if (state !== current) setState(current);

  useEffect(() => {
    if (definitelyOverflowsUserBubble(text) || definitelyFitsUserBubble(text)) return;
    const el = ref.current;
    if (!el) return;
    const measure = (height: number) => {
      const line = parseFloat(getComputedStyle(el).lineHeight) || 22;
      const nextOverflowing = height > line * USER_COLLAPSE_THRESHOLD_LINES + 4;
      setState((prev) => (prev.text === text && prev.overflowing !== nextOverflowing
        ? { ...prev, overflowing: nextOverflowing }
        : prev));
    };
    if (typeof ResizeObserver !== "undefined") {
      const observer = new ResizeObserver((entries) => {
        observer.disconnect();
        const entry = entries[0];
        if (entry) measure(entry.contentRect.height);
      });
      observer.observe(el);
      return () => observer.disconnect();
    }
    const frame = requestAnimationFrame(() => measure(el.getBoundingClientRect().height));
    return () => cancelAnimationFrame(frame);
  }, [text]);

  if (!current.overflowing) {
    return <div ref={ref} className="whitespace-pre-wrap break-words">{text}</div>;
  }
  return (
    <MessageBubbleCollapsible
      open={current.expanded}
      onOpenChange={(nextOpen) => setState((prev) => (prev.text === text ? { ...prev, expanded: nextOpen } : { text, overflowing: true, expanded: nextOpen }))}
      collapsedLines={USER_COLLAPSE_LINES}
      moreLabel="展开全部"
      lessLabel="收起"
      triggerClassName="-mb-1 -ml-2 mt-1 h-6"
    >
      <div ref={ref} className="whitespace-pre-wrap break-words">
        {current.expanded ? text : collapsedUserPreview(text)}
      </div>
    </MessageBubbleCollapsible>
  );
}

function scrollParent(element: HTMLElement): HTMLElement | null {
  for (let node = element.parentElement; node; node = node.parentElement) {
    const { overflowY } = getComputedStyle(node);
    if ((overflowY === "auto" || overflowY === "scroll") && node.scrollHeight > node.clientHeight) return node;
  }
  return null;
}

function anchorRect(range: Range): DOMRect {
  return Array.from(range.getClientRects()).find((rect) => rect.bottom > 0 && rect.top < window.innerHeight) || range.getBoundingClientRect();
}

function AnnotationQuote({ node, index, onJump }: { node: ComposerContextNode; index: number; onJump?: () => void }) {
  const stale = node.status !== "ok";
  const body = (
    <>
      <span className="flex min-w-0 items-start gap-2">
        <span className="cx-annotation-index mt-[3px]">{index}</span>
        <span className="line-clamp-3 min-w-0 flex-1 whitespace-pre-wrap break-words border-l-2 border-cx-border pl-2 text-cx-sm leading-5 text-cx-fg-3">
          {excerptPreview(node.snapshot.text) || node.snapshot.label || node.name}
        </span>
      </span>
      {node.comment ? (
        <span className="block whitespace-pre-wrap break-words pl-6 text-cx-sm leading-5 text-cx-fg">{node.comment}</span>
      ) : null}
      {stale && node.status_reason ? <span className="block pl-6 text-cx-xs text-cx-warning">{node.status_reason}</span> : null}
    </>
  );
  const className = "flex w-full min-w-0 flex-col gap-1 rounded-xl bg-[color-mix(in_srgb,var(--cx-fg)_4%,transparent)] px-2.5 py-2 text-left";
  if (!onJump) return <div className={className} data-annotation-node={node.node_id}>{body}</div>;
  return (
    <button
      type="button"
      onClick={onJump}
      data-annotation-node={node.node_id}
      title="定位到原文"
      aria-label={`定位注释 ${index} 的原文`}
      className={cn(className, "cx-press hover:bg-[color-mix(in_srgb,var(--cx-fg)_7%,transparent)] focus-visible:outline focus-visible:outline-2")}
    >
      {body}
    </button>
  );
}

const FRESH_MESSAGE_MS = 8000;

function isFresh(createdAt?: string): boolean {
  if (!createdAt) return false;
  const time = Date.parse(createdAt);
  return Number.isFinite(time) && Date.now() - time < FRESH_MESSAGE_MS;
}

export function ConversationMessage({
  role,
  sourceProvider,
  sourceContent,
  text,
  messageId = "",
  readingMessageId,
  turnId,
  label,
  isStreaming = false,
  createdAt,
  attachments = [],
  contextRefs = [],
  onCopy,
  onRetry,
  onFork,
  onEdit,
  onShowSentContext,
  onRewind,
  rewindDisabled = true,
  rewindTitle,
  onCiteSelection,
  onAskSideChat,
  citationSources,
  sourceMessages,
  onJumpToContext,
  onOpenAttachment,
  onResourceLink,
  highlightRange = null,
  annotations,
  showIdentity = true,
  showActions = true,
  actionsDisabled = false,
  className = "",
  superseded = false,
  threadId,
  pinActions = false,
  actionMeta,
}: ConversationMessageProps) {
  const { lang } = useLang();
  const contextMenu = useContextMenu();
  const { copy } = useCopy();
  const bodyRef = useRef<HTMLDivElement>(null);
  const citeToolbarRef = useRef<HTMLDivElement>(null);
  const citeRangeRef = useRef<Range | null>(null);
  const highlightOwner = useId();
  const [animateIn] = useState(() => role === "user" && isFresh(createdAt));
  const [citeMenu, setCiteMenu] = useState<{
    top: number;
    left: number;
    text: string;
    start: number;
    end: number;
    messageId: string;
  } | null>(null);
  const [citeComment, setCiteComment] = useState("");
  const [citeError, setCiteError] = useState("");
  const [annotationBadges, setAnnotationBadges] = useState<Array<{ id: string; index: number; top: number; left: number }>>([]);

  const nodes = contextRefs
    .map((item) => normalizeContextNode(item))
    .filter((item): item is ComposerContextNode => Boolean(item));
  const assistantMessageId = messageId || (turnId ? `msg-asst-${turnId}` : "");

  const ownsSourceMessage = useCallback((id: string) => (
    role === "assistant" && Boolean(id) && (citationSources
      ? citationSources.some((span) => span.messageId === id)
      : id === messageId || id === assistantMessageId)
  ), [assistantMessageId, citationSources, messageId, role]);

  const rangeForLocator = useCallback((id: string, start: number, end: number) => {
    const body = bodyRef.current;
    if (!body) return null;
    const display = displayOffsetsForLocator(text, citationSources, id, start, end);
    return display ? messageOffsetsToRange(body, text, display.startOffset, display.endOffset) : null;
  }, [citationSources, text]);
  const rangeForLocatorRef = useRef(rangeForLocator);
  rangeForLocatorRef.current = rangeForLocator;

  const updateSelection = useCallback(() => {
    if ((!onCiteSelection && !onAskSideChat) || role !== "assistant") {
      setCiteMenu(null);
      return;
    }
    if (document.activeElement?.closest("[data-cite-to-composer]")) return;
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || !bodyRef.current || (!bodyRef.current.contains(selection.anchorNode) || !bodyRef.current.contains(selection.focusNode))) {
      setCiteMenu(null);
      setCiteError("");
      return;
    }
    const selected = selection.toString();
    const trimmed = selected.trim();
    if (!trimmed) {
      setCiteMenu(null);
      setCiteError("");
      return;
    }
    const range = selection.getRangeAt(0);
    const offsets = messageRangeOffsets(bodyRef.current, range, text);
    if (!offsets) { setCiteMenu(null); setCiteError(lang === "en" ? "This selection contains content without an exact source location. Select prose or code." : "此选区包含无法精确定位的内容，请重新选择正文或代码。"); return; }
    const source = citationSources?.find((span) => offsets.startOffset >= span.start && offsets.endOffset <= span.end);
    if ((citationSources && !source) || (!citationSources && !messageId)) {
      setCiteMenu(null);
      setCiteError(lang === "en" ? "Select text within one source message." : "请选择同一条来源消息内的文字。");
      return;
    }
    const persistedOffsets = sourceCodePointOffsets(text.slice(source?.start || 0), {
      startOffset: offsets.startOffset - (source?.start || 0),
      endOffset: offsets.endOffset - (source?.start || 0),
    });
    setCiteError("");
    citeRangeRef.current = range.cloneRange();
    const rect = anchorRect(range);
    const start = (source?.sourceStart || 0) + persistedOffsets.startOffset;
    const end = (source?.sourceStart || 0) + persistedOffsets.endOffset;
    const sourceMessageId = source?.messageId || messageId;
    setCiteMenu((current) => {
      if (!current || current.start !== start || current.end !== end || current.messageId !== sourceMessageId) setCiteComment("");
      return { top: rect.top, left: rect.left + rect.width / 2, text: offsets.text, start, end, messageId: sourceMessageId };
    });
  }, [citationSources, messageId, onCiteSelection, onAskSideChat, role, text, lang]);

  useEffect(() => {
    if (role !== "assistant" || (!onCiteSelection && !onAskSideChat)) return;
    let frame = 0;
    let selecting = false;
    const inToolbar = (target: EventTarget | null) => target instanceof Element && Boolean(target.closest("[data-cite-to-composer]"));
    const schedule = () => {
      cancelAnimationFrame(frame);
      if (selecting || document.activeElement?.closest("[data-cite-to-composer]")) return;
      frame = requestAnimationFrame(updateSelection);
    };
    const down = (event: PointerEvent) => {
      if (inToolbar(event.target)) return;
      selecting = true;
      setCiteMenu(null); setCiteError("");
    };
    const up = (event: PointerEvent) => {
      selecting = false;
      if (!inToolbar(event.target)) schedule();
    };
    const cancel = () => { selecting = false; schedule(); };
    document.addEventListener("pointerdown", down);
    document.addEventListener("pointerup", up);
    document.addEventListener("pointercancel", cancel);
    document.addEventListener("selectionchange", schedule);
    return () => {
      cancelAnimationFrame(frame);
      document.removeEventListener("pointerdown", down);
      document.removeEventListener("pointerup", up);
      document.removeEventListener("pointercancel", cancel);
      document.removeEventListener("selectionchange", schedule);
    };
  }, [onCiteSelection, onAskSideChat, role, updateSelection]);

  const citeOpen = Boolean(citeMenu);
  useEffect(() => {
    if (!citeOpen) return;
    let frame = 0;
    const follow = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        const range = citeRangeRef.current;
        if (!range || !range.startContainer.isConnected) { setCiteMenu(null); return; }
        const rect = anchorRect(range);
        if (rect.bottom < 0 || rect.top > window.innerHeight) {
          if (!document.activeElement?.closest("[data-cite-to-composer]")) setCiteMenu(null);
          return;
        }
        setCiteMenu((current) => current && (current.top !== rect.top || current.left !== rect.left + rect.width / 2)
          ? { ...current, top: rect.top, left: rect.left + rect.width / 2 }
          : current);
      });
    };
    const enterToolbar = (event: KeyboardEvent) => {
      if (event.key !== "Tab" || event.shiftKey || citeToolbarRef.current?.contains(event.target as Node)) return;
      const selection = window.getSelection();
      if (!selection || !bodyRef.current?.contains(selection.anchorNode) || !bodyRef.current.contains(selection.focusNode)) return;
      event.preventDefault();
      citeToolbarRef.current?.querySelector<HTMLElement>("input, button")?.focus({ preventScroll: true });
    };
    window.addEventListener("scroll", follow, true);
    window.addEventListener("resize", follow);
    document.addEventListener("keydown", enterToolbar);
    return () => {
      cancelAnimationFrame(frame);
      window.removeEventListener("scroll", follow, true);
      window.removeEventListener("resize", follow);
      document.removeEventListener("keydown", enterToolbar);
    };
  }, [citeOpen]);

  useEffect(() => {
    const range = citeOpen ? citeRangeRef.current : null;
    setTextHighlight("cx-annotation-draft", highlightOwner, range ? [range] : []);
    return () => setTextHighlight("cx-annotation-draft", highlightOwner, []);
  }, [citeOpen, citeMenu?.start, citeMenu?.end, highlightOwner]);

  const ownAnnotations = (annotations || []).filter((mark) => ownsSourceMessage(mark.messageId));
  const ownAnnotationsRef = useRef(ownAnnotations);
  ownAnnotationsRef.current = ownAnnotations;
  const annotationKey = ownAnnotations.map((mark) => `${mark.id}:${mark.index}:${mark.messageId}:${mark.startOffset}-${mark.endOffset}`).join("|");
  useEffect(() => {
    const body = bodyRef.current;
    if (!body || !annotationKey) {
      setAnnotationBadges((current) => (current.length ? [] : current));
      return;
    }
    let frame = 0;
    const layout = () => {
      const bodyRect = body.getBoundingClientRect();
      const ranges: Range[] = [];
      const badges: Array<{ id: string; index: number; top: number; left: number }> = [];
      for (const mark of ownAnnotationsRef.current) {
        const range = rangeForLocatorRef.current(mark.messageId, mark.startOffset, mark.endOffset);
        if (!range) continue;
        ranges.push(range);
        const last = Array.from(range.getClientRects()).filter((rect) => rect.width > 0).at(-1);
        if (last) badges.push({ id: mark.id, index: mark.index, top: last.top - bodyRect.top, left: last.right - bodyRect.left });
      }
      setTextHighlight("cx-annotation", highlightOwner, ranges);
      setAnnotationBadges(badges);
    };
    const schedule = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(layout);
    };
    schedule();
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(schedule);
    observer?.observe(body);
    return () => {
      cancelAnimationFrame(frame);
      observer?.disconnect();
      setTextHighlight("cx-annotation", highlightOwner, []);
    };
  }, [annotationKey, highlightOwner, text]);

  const jumpKey = highlightRange && ownsSourceMessage(highlightRange.messageId) ? highlightRange.key : "";
  const jumpRangeRef = useRef(highlightRange);
  jumpRangeRef.current = highlightRange;
  useEffect(() => {
    const body = bodyRef.current;
    const target = jumpRangeRef.current;
    if (!jumpKey || !body || !target) return;
    const row = body.parentElement;
    if (row) row.dataset.citeJump = jumpKey;
    const resolve = () => (target.endOffset > target.startOffset
      ? rangeForLocatorRef.current(target.messageId, target.startOffset, target.endOffset)
      : null);
    let range = resolve();
    const scroller = scrollParent(body);
    const reveal = () => {
      if (!scroller) return false;
      const box = scroller.getBoundingClientRect();
      const rect = range ? range.getBoundingClientRect() : body.getBoundingClientRect();
      const delta = range
        ? rect.top - box.top - (box.height - Math.min(rect.height, box.height)) / 2
        : rect.top - box.top - 24;
      if (Math.abs(delta) <= 1) return false;
      scroller.scrollTop += delta;
      return true;
    };
    let frame = 0, frames = 0, stable = 0, stopped = false;
    const stop = () => { stopped = true; cancelAnimationFrame(frame); };
    const tick = () => {
      if (stopped) return;
      if (range && !range.startContainer.isConnected) range = resolve();
      stable = reveal() ? 0 : stable + 1;
      if (stable < 3 && ++frames < 40) frame = requestAnimationFrame(tick);
    };
    tick();
    const userEvents = ["wheel", "touchstart", "keydown", "pointerdown"] as const;
    for (const type of userEvents) scroller?.addEventListener(type, stop, { passive: true });
    if (range) setTextHighlight("cx-cite-flash", highlightOwner, [range]);
    else body.classList.add("is-cite-highlight");
    const timer = window.setTimeout(() => {
      setTextHighlight("cx-cite-flash", highlightOwner, []);
      body.classList.remove("is-cite-highlight");
    }, 2400);
    return () => {
      stop();
      for (const type of userEvents) scroller?.removeEventListener(type, stop);
      window.clearTimeout(timer);
      setTextHighlight("cx-cite-flash", highlightOwner, []);
      body.classList.remove("is-cite-highlight");
      if (row?.dataset.citeJump === jumpKey) delete row.dataset.citeJump;
    };
  }, [jumpKey, highlightOwner]);

  const submitCite = (action: NonNullable<ConversationMessageProps["onCiteSelection"]>) => {
    if (!citeMenu) return;
    const comment = citeComment.trim();
    action({
      messageId: citeMenu.messageId || assistantMessageId, turnId, text: citeMenu.text,
      startOffset: citeMenu.start, endOffset: citeMenu.end, streaming: isStreaming,
      ...(comment ? { comment } : {}),
    });
    setCiteMenu(null);
    setCiteComment("");
    window.getSelection()?.removeAllRanges();
  };

  if (role !== "user" && role !== "assistant") {
    const sourceLabel = label || (role === "tool" ? "工具记录" : `原始角色：${role || "unknown"}`);
    return <section className={cn("rounded-lg border border-cx-border-subtle bg-cx-bg-subtle px-3 py-2 text-cx-fg-3", className)} data-message-id={messageId || undefined} aria-label={sourceLabel}>
      <p className="mb-1 text-cx-xs font-medium">{sourceLabel}{sourceProvider ? ` · ${sourceProvider}` : ""}{superseded ? " · 已替代" : ""}</p>
      <pre className="whitespace-pre-wrap break-words font-cx-mono text-cx-xs">{text}</pre>
      {sourceContent !== undefined ? <details className="mt-2"><summary className="cursor-pointer text-cx-xs">原始内容块</summary><pre className="whitespace-pre-wrap break-words font-cx-mono text-cx-xs">{JSON.stringify(sourceContent, null, 2)}</pre></details> : null}
    </section>;
  }

  if (role === "user") {
    const canShowMenu = showActions && !superseded;
    const menuItems = <>
      <MenuItem icon="copy" onSelect={() => { if (onCopy) onCopy(); else void copy(text); }}>复制消息</MenuItem>
      {onShowSentContext ? <MenuItem icon="eye" onSelect={onShowSentContext}>查看实际发送内容</MenuItem> : null}
      {onEdit ? <><MenuSeparator /><MenuItem icon="edit" disabled={actionsDisabled} onSelect={onEdit}>从此处编辑</MenuItem></> : null}
      {onFork ? <MenuItem icon="gitBranch" disabled={actionsDisabled} onSelect={onFork}>从这里派生</MenuItem> : null}
      {onRewind ? (
        <MenuItem
          icon="undo"
          disabled={actionsDisabled || rewindDisabled}
          description={rewindDisabled ? rewindTitle || "原生回退不可用" : undefined}
          onSelect={onRewind}
        >
          回退到这里
        </MenuItem>
      ) : null}
    </>;
    const time = formatMessageTime(createdAt);
    const annotationNodes = nodes.filter((node) => node.kind === "message_span");
    const chipNodes = nodes.filter((node) => node.kind !== "message_span");
    const inlineImage = (attachment: MessageAttachmentChip) => Boolean(threadId && attachment.artifactId && isImageAttachment(attachment));
    const imageAttachments = attachments.filter(inlineImage).map((attachment) => ({ ref: attachment.artifactId!, name: attachment.name }));
    const fileAttachments = attachments.filter((attachment) => !inlineImage(attachment));
    return (
      <Message
        from="user"
        animateIn={animateIn}
        aria-label="用户消息"
        className={cn("user group/msg", className)}
        data-message-id={messageId || undefined}
        tabIndex={canShowMenu ? 0 : undefined}
        aria-haspopup={canShowMenu ? "menu" : undefined}
        onContextMenu={canShowMenu ? (event) => {
          if (event.target instanceof Element && event.target.closest("a[href], img, [data-testid='message-attachment-chip']")) return;
          contextMenu.onContextMenu(event);
        } : undefined}
        onKeyDown={canShowMenu ? contextMenu.onKeyDown : undefined}
      >
        <MessageContent className="gap-1">
          {imageAttachments.length > 0 ? (
            <MessageImages threadId={threadId!} items={imageAttachments} className="max-w-[82%]" />
          ) : null}
          {fileAttachments.length > 0 ? (
            <div className="flex max-w-[82%] flex-wrap justify-end gap-1.5">
              {fileAttachments.map((attachment, index) => (
                <AttachmentChip
                  key={attachment.artifactId || `${attachment.name}-${index}`}
                  attachment={attachment}
                  onOpen={attachment.artifactId && onOpenAttachment ? () => onOpenAttachment(attachment) : undefined}
                />
              ))}
            </div>
          ) : null}
          <MessageBubble variant={superseded ? "outline" : "soft"} animateIn={animateIn}>
            <MessageBubbleContent
              className={cn(
                "cx-user-bubble rounded-3xl rounded-br-md px-4 py-2.5 text-[length:var(--conv-fs-body,14px)] leading-[1.6]",
                superseded ? "text-cx-fg-3" : "text-cx-fg",
              )}
            >
              {label || superseded ? (
                <div className="mb-1 text-cx-xs font-medium text-cx-accent">{superseded ? "已替代" : label}</div>
              ) : null}
              {annotationNodes.length > 0 ? (
                <div className="mb-2 flex flex-col gap-1.5" data-testid="message-annotations">
                  {annotationNodes.map((node, index) => (
                    <AnnotationQuote
                      key={node.node_id}
                      node={node}
                      index={index + 1}
                      onJump={node.locator.message_id && onJumpToContext
                        ? () => onJumpToContext({
                          messageId: String(node.locator.message_id),
                          turnId: node.locator.turn_id,
                          startOffset: node.locator.start_offset,
                          endOffset: node.locator.end_offset,
                        })
                        : undefined}
                    />
                  ))}
                </div>
              ) : null}
              {chipNodes.length > 0 ? (
                <div className="mb-2 flex flex-wrap gap-1">
                  {chipNodes.map((node) => <ContextNodeChip key={node.node_id} node={node} />)}
                </div>
              ) : null}
              <UserBubbleText text={text} />
            </MessageBubbleContent>
          </MessageBubble>
          {showActions && !superseded ? (
            <MessageActionBar align="end" meta={time || undefined} className="-mr-1">
              <CopyMessageButton text={text} label="复制消息" />
              {onShowSentContext ? (
                <MessageActionButton icon="eye" label="查看实际发送内容" onClick={onShowSentContext} testId="turn-sent-context" />
              ) : null}
              {onEdit ? (
                <MessageActionButton icon="edit" label="从此处编辑" onClick={onEdit} disabled={actionsDisabled} testId="c11-edit-user" />
              ) : null}
              <Menu ariaLabel="用户消息操作" trigger={<IconButton icon="more" label="更多消息操作" size="sm" className="size-7 rounded-lg" />}>{menuItems}</Menu>
            </MessageActionBar>
          ) : null}
        </MessageContent>
        {canShowMenu ? contextMenu.render(menuItems, "用户消息操作") : null}
      </Message>
    );
  }

  const time = formatMessageTime(createdAt);
  const meta = actionMeta ?? (time || undefined);

  return (
    <div
      className={cn("assistant flex w-full min-w-0 flex-col", className)}
      data-message-id={assistantMessageId || undefined}
      data-reading-message-id={readingMessageId}
      data-citation-message-ids={citationSources?.map(source => source.messageId).join(" ") || undefined}
    >
      <div
        ref={bodyRef}
        className="ai-message-body relative text-[length:var(--conv-fs-body,14px)] text-cx-fg"
        data-cite-body={onCiteSelection || onAskSideChat ? "1" : undefined}
      >
        {citeMenu ? <Popover open anchorPoint={{ x: citeMenu.left, y: citeMenu.top }} placement="top" initialFocus="none" restoreFocus={false}
          role="dialog" ariaLabel={lang === "en" ? "Annotate selected text" : "注释选中文字"}
          onOpenChange={(open) => { if (!open) setCiteMenu(null); }}
          className="min-w-0 border border-cx-border p-1" style={{ maxWidth: "calc(100vw - 16px)" }}>
          <div
            ref={citeToolbarRef}
            data-cite-to-composer="1"
            className="flex w-[min(340px,calc(100vw-32px))] flex-col text-cx-sm text-cx-fg"
            onMouseDown={(event) => { if (!(event.target instanceof HTMLInputElement)) event.preventDefault(); }}
            onKeyDown={(event) => { if (event.key === "Escape") { event.stopPropagation(); setCiteMenu(null); window.getSelection()?.removeAllRanges(); } }}
          >
            {onCiteSelection ? (
              <form
                className="flex items-center gap-1"
                onSubmit={(event) => { event.preventDefault(); submitCite(onCiteSelection); }}
              >
                <input
                  value={citeComment}
                  onChange={(event) => setCiteComment(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" && (event.nativeEvent.isComposing || event.keyCode === 229)) event.preventDefault();
                  }}
                  placeholder={lang === "en" ? "Add an optional comment…" : "添加可选评论…"}
                  aria-label={lang === "en" ? "Comment on the selected text" : "对选中文字的评论"}
                  data-testid="cite-comment-input"
                  className="h-8 min-w-0 flex-1 rounded-md bg-transparent px-2 text-cx-sm text-cx-fg outline-none placeholder:text-cx-fg-4"
                />
                <button
                  type="submit"
                  aria-label={lang === "en" ? "Add annotation to the message" : "添加注释到对话"}
                  title={lang === "en" ? "Add annotation" : "添加注释"}
                  data-testid="cite-add-annotation"
                  className="grid size-7 shrink-0 place-items-center rounded-full bg-cx-fg text-cx-bg transition-opacity hover:opacity-85 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2"
                >
                  <Icon name="check" size={14} />
                </button>
              </form>
            ) : null}
            {onAskSideChat ? (
              <button
                type="button"
                className={cn(
                  "flex h-7 items-center gap-1.5 rounded-md px-2 text-cx-xs text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg focus-visible:outline focus-visible:outline-2",
                  onCiteSelection && "mt-1",
                )}
                onClick={() => submitCite(onAskSideChat)}
              >
                <Icon name="messageCircle" size={13} />
                {lang === "en" ? "Ask in side chat" : "在侧边聊天中提问"}
              </button>
            ) : null}
          </div>
        </Popover> : null}
        {annotationBadges.map((badge) => (
          <span
            key={badge.id}
            aria-hidden="true"
            className="cx-annotation-badge"
            style={{ top: badge.top, left: badge.left }}
          >
            {badge.index}
          </span>
        ))}
        {citeError ? <p role="status" className="text-cx-xs text-cx-fg-4">{citeError}</p> : null}
        <StreamingText
          text={text}
          isStreaming={isStreaming}
          onCopy={onCopy}
          onRetry={onRetry}
          onFork={onFork}
          onRewind={onRewind}
          rewindDisabled={rewindDisabled}
          rewindTitle={rewindTitle}
          showActions={showActions}
          actionsDisabled={actionsDisabled}
          onResourceLink={onResourceLink}
          threadId={threadId}
          messageId={showIdentity ? assistantMessageId || undefined : undefined}
          visualizationMessageId={readingMessageId ?? messageId}
          visualizationSources={citationSources ? { spans: citationSources, messages: sourceMessages } : undefined}
          meta={meta}
          pinActions={pinActions}
        />
      </div>
    </div>
  );
}
