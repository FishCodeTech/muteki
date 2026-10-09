"use client";

/* ─────────────────────────────────────────────────────────
 * STREAMING TEXT — Markdown stream with citations & message actions.
 *
 * Originally adapted from https://github.com/TurboKach/ai-native-react-components
 * (MIT, pinned 05dab2d2b5f1f3e40029776e339a486d70491079).
 * ───────────────────────────────────────────────────────── */

import React, { useCallback, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Collapse, IconButton, Menu, MenuItem, toast, useCopy } from "@/components/chat/ui";
import { ChatMarkdown } from "@/components/chat/markdown/ChatMarkdown";
import {
  CopyMessageButton,
  MessageActionBar,
  MessageActionButton,
  messageDeepLink,
} from "@/components/chat/timeline/MessageActions";
import type { ResourceLinkTarget } from "@/lib/resourcePreview";

export interface Citation {
  id?: string;
  name: string;
  source?: string;
  url?: string;
  kind?: string;
  onClick?: () => void;
}

export type MessageViewMode = "rendered" | "source";

export interface StreamingTextProps {
  text: string;
  isStreaming?: boolean;
  citations?: Citation[];
  onCopy?: () => void;
  onRetry?: () => void;
  onFork?: () => void;
  onRewind?: () => void;
  rewindDisabled?: boolean;
  rewindTitle?: string;
  showActions?: boolean;
  actionsDisabled?: boolean;
  className?: string;
  onResourceLink?: (target: ResourceLinkTarget) => void;
  threadId?: string;
  /** Enables the copy-link action. */
  messageId?: string;
  /** Muted trailing text in the action row (time, duration). */
  meta?: React.ReactNode;
  /** Keep the action row visible (latest reply). */
  pinActions?: boolean;
}

export interface StreamingTextActionsProps {
  text?: string;
  onCopy?: () => void;
  onRetry?: () => void;
  onFork?: () => void;
  onRewind?: () => void;
  rewindDisabled?: boolean;
  rewindTitle?: string;
  disabled?: boolean;
  showCopy?: boolean;
  viewMode?: MessageViewMode;
  onToggleViewMode?: () => void;
  showViewModeToggle?: boolean;
  className?: string;
  messageId?: string;
  meta?: React.ReactNode;
  pinned?: boolean;
}

function StreamingTextActions({
  text = "",
  onCopy,
  onRetry,
  onFork,
  onRewind,
  rewindDisabled = true,
  rewindTitle = "",
  disabled = false,
  showCopy = true,
  viewMode = "rendered",
  onToggleViewMode,
  showViewModeToggle = false,
  className = "",
  messageId,
  meta,
  pinned = false,
}: StreamingTextActionsProps) {
  const { copy } = useCopy();
  const hasMore = Boolean(onRewind || onFork || messageId || (showViewModeToggle && onToggleViewMode));
  return (
    <MessageActionBar className={className} meta={meta} pinned={pinned}>
      {showCopy ? (
        onCopy ? (
          <MessageActionButton icon="copy" label="复制回复" onClick={onCopy} disabled={disabled} />
        ) : (
          <CopyMessageButton text={text} label="复制回复" disabled={disabled} />
        )
      ) : null}
      {onRetry ? (
        <MessageActionButton icon="retry" label="重新生成" onClick={onRetry} disabled={disabled} testId="c11-retry" />
      ) : null}
      {hasMore ? (
        <Menu
          placement="bottom-start"
          ariaLabel="更多消息操作"
          trigger={<IconButton icon="more" label="更多消息操作" size="sm" className="size-7 rounded-lg" />}
        >
          {onRewind ? (
            <MenuItem
              icon="undo"
              disabled={disabled || rewindDisabled}
              description={disabled || rewindDisabled ? rewindTitle || "原生回退不可用" : undefined}
              onSelect={onRewind}
            >
              回退到此处
            </MenuItem>
          ) : null}
          {onFork ? <MenuItem icon="gitFork" disabled={disabled} onSelect={onFork}>从此处分叉对话</MenuItem> : null}
          {messageId ? (
            <MenuItem
              icon="link"
              onSelect={() => {
                void copy(messageDeepLink(messageId)).then((ok) => {
                  if (ok) toast({ title: "已复制消息链接", tone: "success" });
                });
              }}
            >
              复制消息链接
            </MenuItem>
          ) : null}
          {showViewModeToggle && onToggleViewMode ? (
            <MenuItem icon={viewMode === "rendered" ? "code" : "eye"} onSelect={onToggleViewMode}>
              {viewMode === "rendered" ? "查看 Markdown 原文" : "查看渲染结果"}
            </MenuItem>
          ) : null}
        </Menu>
      ) : null}
    </MessageActionBar>
  );
}

export function StreamingText({
  text,
  isStreaming = false,
  citations = [],
  onCopy,
  onRetry,
  onFork,
  onRewind,
  rewindDisabled = true,
  rewindTitle,
  showActions = true,
  actionsDisabled = false,
  className = "",
  onResourceLink,
  threadId,
  messageId,
  meta,
  pinActions = false,
}: StreamingTextProps) {
  const [sourcesOpen, setSourcesOpen] = useState(false);
  const [viewMode, setViewMode] = useState<MessageViewMode>("rendered");
  const toggleViewMode = useCallback(() => {
    setViewMode((current) => (current === "rendered" ? "source" : "rendered"));
  }, []);
  const hasText = Boolean(text.trim());
  const showMessageActions = showActions && !isStreaming;

  return (
    <div className={cn("group/msg flex w-full flex-col", className)}>
      <div aria-busy={isStreaming || undefined}>
        {viewMode === "source" && !isStreaming ? (
          <pre className="m-0 whitespace-pre-wrap break-words rounded-xl bg-cx-sunken px-4 py-3 font-cx-mono text-[13px] leading-6 text-cx-fg-2">
            {text}
          </pre>
        ) : (
          <ChatMarkdown text={text} streaming={isStreaming} threadId={threadId} messageId={messageId} onResourceLink={onResourceLink} />
        )}
      </div>

      {citations.length > 0 ? (
        <div className="mt-3 flex flex-col gap-1.5">
          <button
            type="button"
            aria-expanded={sourcesOpen}
            onClick={() => setSourcesOpen((current) => !current)}
            className="cx-press inline-flex h-7 w-fit items-center gap-1.5 rounded-full border border-cx-border px-2.5 text-[12px] font-medium text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg"
          >
            <Icon name="book" size={13} />
            {citations.length} 个参考来源
            <Icon name="chevronDown" size={12} className={cn("transition-transform duration-150", sourcesOpen && "rotate-180")} />
          </button>
          <Collapse open={sourcesOpen}>
            <ul className="m-0 flex list-none flex-col rounded-xl border border-cx-border-subtle p-1">
              {citations.map((citation, index) => (
                <li key={citation.id || index} className="flex items-center gap-2 rounded-lg px-2 py-1.5 text-[13px] hover:bg-cx-hover">
                  <span className="min-w-0 flex-1 truncate font-medium text-cx-fg">{citation.name}</span>
                  {citation.source ? <span className="font-cx-mono text-[12px] text-cx-fg-4">{citation.source}</span> : null}
                  {citation.onClick ? (
                    <button type="button" onClick={citation.onClick} className="text-[12px] font-medium text-cx-accent hover:underline">
                      查看
                    </button>
                  ) : null}
                </li>
              ))}
            </ul>
          </Collapse>
        </div>
      ) : null}

      {hasText && showMessageActions ? (
        <StreamingTextActions
          text={text}
          onCopy={onCopy}
          onRetry={onRetry}
          onFork={onFork}
          onRewind={onRewind}
          rewindDisabled={rewindDisabled}
          rewindTitle={rewindTitle}
          disabled={actionsDisabled}
          viewMode={viewMode}
          onToggleViewMode={toggleViewMode}
          showViewModeToggle
          messageId={messageId}
          meta={meta}
          pinned={pinActions}
          className="mt-1.5 -ml-1.5"
        />
      ) : null}
    </div>
  );
}
