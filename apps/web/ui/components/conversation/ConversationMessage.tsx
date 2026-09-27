"use client";

import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { prefersReducedMotion } from "@/lib/usePrefersReducedMotion";
import { StreamingText } from "../ai-native/streaming-text";
import { Icon, type IconName } from "../Icon";
import { ContextNodeChip } from "./ComposerPromptDocument";
import { normalizeContextNode, type ComposerContextNode } from "@/lib/composerContextDoc";
import type { ResourceLinkTarget } from "@/lib/resourcePreview";
import {
  Message,
  MessageBubble,
  MessageBubbleCollapsible,
  MessageBubbleContent,
  MessageContent,
} from "@/components/agentui/agents/message";
import {
  CopyMessageButton,
  MessageActionBar,
  MessageActionButton,
  formatMessageTime,
} from "@/components/chat/timeline/MessageActions";

export interface MessageAttachmentChip {
  sha256?: string;
  name: string;
  size?: number;
  mediaType?: string;
  kind?: string;
}

export interface ConversationMessageProps {
  role: "user" | "assistant" | "system";
  text: string;
  messageId?: string;
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
  }) => void;
  onJumpToContext?: (payload: {
    messageId: string;
    startOffset?: number;
    endOffset?: number;
  }) => void;
  onOpenAttachment?: (attachment: MessageAttachmentChip) => void;
  onResourceLink?: (target: ResourceLinkTarget) => void;
  highlightRange?: { startOffset: number; endOffset: number } | null;
  showIdentity?: boolean;
  showActions?: boolean;
  actionsDisabled?: boolean;
  className?: string;
  superseded?: boolean;
  threadId?: string;
  /** Keep the assistant action row visible (latest reply). */
  pinActions?: boolean;
  /** Extra muted text in the assistant action row, e.g. turn duration. */
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
        <span className="max-w-[180px] truncate text-[12.5px] font-medium leading-4 text-cx-fg">{attachment.name}</span>
        {size ? <span className="text-[11px] leading-4 text-cx-fg-4">{size}</span> : null}
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

function UserBubbleText({ text }: { text: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [overflowing, setOverflowing] = useState(false);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const line = parseFloat(getComputedStyle(el).lineHeight) || 22;
    // Only clamp when hiding at least a couple of lines is worth the extra click.
    setOverflowing(el.scrollHeight > line * (USER_COLLAPSE_LINES + 2) + 4);
  }, [text]);
  const body = <div ref={ref} className="whitespace-pre-wrap break-words">{text}</div>;
  if (!overflowing) return body;
  return (
    <MessageBubbleCollapsible
      collapsedLines={USER_COLLAPSE_LINES}
      moreLabel="展开全部"
      lessLabel="收起"
      triggerClassName="-mb-1 -ml-2 mt-1 h-6"
    >
      {body}
    </MessageBubbleCollapsible>
  );
}

/** Rows younger than this pop in; history loaded on open renders still. */
const FRESH_MESSAGE_MS = 8000;

function isFresh(createdAt?: string): boolean {
  if (!createdAt) return false;
  const time = Date.parse(createdAt);
  return Number.isFinite(time) && Date.now() - time < FRESH_MESSAGE_MS;
}

export function ConversationMessage({
  role,
  text,
  messageId = "",
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
  onRewind,
  rewindDisabled = true,
  rewindTitle,
  onCiteSelection,
  onJumpToContext,
  onOpenAttachment,
  onResourceLink,
  highlightRange = null,
  showIdentity = true,
  showActions = true,
  actionsDisabled = false,
  className = "",
  superseded = false,
  threadId,
  pinActions = false,
  actionMeta,
}: ConversationMessageProps) {
  const bodyRef = useRef<HTMLDivElement>(null);
  const [animateIn] = useState(() => role === "user" && isFresh(createdAt));
  const [citeMenu, setCiteMenu] = useState<{
    top: number;
    left: number;
    text: string;
    start: number;
    end: number;
    messageId: string;
  } | null>(null);

  const nodes = contextRefs
    .map((item) => normalizeContextNode(item))
    .filter((item): item is ComposerContextNode => Boolean(item));
  const assistantMessageId = messageId || (turnId ? `msg-asst-${turnId}` : "");

  const handleMouseUp = useCallback(() => {
    if (!onCiteSelection || !assistantMessageId || role !== "assistant") {
      setCiteMenu(null);
      return;
    }
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || !bodyRef.current || !bodyRef.current.contains(selection.anchorNode)) {
      setCiteMenu(null);
      return;
    }
    const selected = selection.toString();
    const trimmed = selected.trim();
    if (!trimmed || trimmed.length < 2) {
      setCiteMenu(null);
      return;
    }
    let start = text.indexOf(selected);
    if (start < 0) {
      const visible = bodyRef.current.innerText || text;
      start = visible.indexOf(selected);
      if (start < 0) start = visible.indexOf(trimmed);
      if (start < 0) {
        setCiteMenu(null);
        return;
      }
    }
    const range = selection.getRangeAt(0);
    const rect = range.getBoundingClientRect();
    const host = bodyRef.current.getBoundingClientRect();
    setCiteMenu({
      top: Math.max(-36, rect.top - host.top - 40),
      left: Math.max(0, Math.min(rect.left - host.left + rect.width / 2 - 64, Math.max(0, host.width - 132))),
      text: selected,
      start,
      end: start + selected.length,
      messageId: assistantMessageId,
    });
  }, [assistantMessageId, onCiteSelection, role, text]);

  useEffect(() => {
    if (!citeMenu) return;
    const clear = (event: PointerEvent) => {
      if ((event.target as Element | null)?.closest?.("[data-cite-to-composer]")) return;
      if (!window.getSelection()?.toString()) setCiteMenu(null);
    };
    document.addEventListener("pointerup", clear);
    return () => document.removeEventListener("pointerup", clear);
  }, [citeMenu]);

  useEffect(() => {
    if (!highlightRange || !bodyRef.current || role !== "assistant") return;
    bodyRef.current.scrollIntoView({ behavior: prefersReducedMotion() ? "instant" : "smooth", block: "center" });
    bodyRef.current.classList.add("is-cite-highlight");
    const timer = window.setTimeout(() => {
      bodyRef.current?.classList.remove("is-cite-highlight");
    }, 1600);
    return () => window.clearTimeout(timer);
  }, [highlightRange, role]);

  if (role === "user") {
    const time = formatMessageTime(createdAt);
    return (
      <Message
        from="user"
        animateIn={animateIn}
        aria-label="用户消息"
        className={cn("user group/msg", className)}
        data-message-id={messageId || undefined}
      >
        <MessageContent className="gap-1">
          {attachments.length > 0 ? (
            <div className="flex max-w-[82%] flex-wrap justify-end gap-1.5">
              {attachments.map((attachment, index) => (
                <AttachmentChip
                  key={attachment.sha256 || `${attachment.name}-${index}`}
                  attachment={attachment}
                  onOpen={attachment.sha256 && onOpenAttachment ? () => onOpenAttachment(attachment) : undefined}
                />
              ))}
            </div>
          ) : null}
          <MessageBubble variant={superseded ? "outline" : "soft"} animateIn={animateIn}>
            <MessageBubbleContent
              className={cn(
                "cx-user-bubble rounded-[20px] rounded-br-md px-4 py-2.5 text-[length:var(--conv-fs-body,14px)] leading-[1.6]",
                superseded ? "text-cx-fg-3" : "text-cx-fg",
              )}
            >
              {label || superseded ? (
                <div className="mb-1 text-[11px] font-medium text-cx-accent">{superseded ? "已替代" : label}</div>
              ) : null}
              {nodes.length > 0 ? (
                <div className="mb-2 flex flex-wrap gap-1">
                  {nodes.map((node) => (
                    <ContextNodeChip
                      key={node.node_id}
                      node={node}
                      onJump={
                        node.kind === "message_span" && node.locator.message_id
                          ? () => onJumpToContext?.({
                            messageId: String(node.locator.message_id),
                            startOffset: node.locator.start_offset,
                            endOffset: node.locator.end_offset,
                          })
                          : undefined
                      }
                    />
                  ))}
                </div>
              ) : null}
              <UserBubbleText text={text} />
            </MessageBubbleContent>
          </MessageBubble>
          {showActions && !superseded ? (
            <MessageActionBar align="end" meta={time || undefined} className="-mr-1">
              <CopyMessageButton text={text} label="复制消息" />
              {onEdit ? (
                <MessageActionButton icon="edit" label="编辑并重发" onClick={onEdit} disabled={actionsDisabled} testId="c11-edit-user" />
              ) : null}
            </MessageActionBar>
          ) : null}
        </MessageContent>
      </Message>
    );
  }

  const time = formatMessageTime(createdAt);
  const meta = actionMeta ?? (time || undefined);

  return (
    <div
      className={cn("assistant flex w-full min-w-0 flex-col", className)}
      data-message-id={assistantMessageId || undefined}
    >
      <div
        ref={bodyRef}
        className="ai-message-body relative text-[length:var(--conv-fs-body,14px)] text-cx-fg"
        data-cite-body={onCiteSelection ? "1" : undefined}
        onMouseUp={handleMouseUp}
      >
        {citeMenu && onCiteSelection ? (
          <button
            type="button"
            data-cite-to-composer="1"
            className="cx-animate-in absolute z-20 inline-flex h-8 items-center gap-1.5 rounded-lg bg-[color-mix(in_srgb,var(--ink)_92%,var(--page))] px-2.5 text-[12.5px] font-medium text-[var(--page)] shadow-cx-md"
            style={{ top: citeMenu.top, left: citeMenu.left }}
            onMouseDown={(event) => event.preventDefault()}
            onClick={() => {
              onCiteSelection({
                messageId: citeMenu.messageId || assistantMessageId,
                turnId,
                text: citeMenu.text,
                startOffset: citeMenu.start,
                endOffset: citeMenu.end,
                streaming: isStreaming,
              });
              setCiteMenu(null);
              window.getSelection()?.removeAllRanges();
            }}
          >
            <Icon name="quote" size={13} />
            引用到输入框
          </button>
        ) : null}
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
          meta={meta}
          pinActions={pinActions}
        />
      </div>
    </div>
  );
}
