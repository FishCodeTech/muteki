"use client";

import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import type { IconName } from "@/components/Icon";
import { IconButton, toast, useCopy } from "@/components/chat/ui";

export function messageDeepLink(messageId: string): string {
  const url = new URL(window.location.href);
  url.searchParams.set("message", messageId);
  url.hash = "";
  return url.toString();
}

export function formatMessageTime(value?: string): string {
  if (!value) return "";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  const now = new Date();
  const sameDay = date.toDateString() === now.toDateString();
  const time = date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (sameDay) return time;
  return `${date.toLocaleDateString([], { month: "numeric", day: "numeric" })} ${time}`;
}

/**
 * Icon action that stays hoverable while disabled (aria-disabled instead of
 * `disabled`) so the tooltip can still explain why, e.g. rewind unavailable.
 */
export function MessageActionButton({
  icon,
  label,
  onClick,
  disabled = false,
  disabledReason,
  testId,
  className,
}: {
  icon: IconName;
  label: string;
  onClick: () => void;
  disabled?: boolean;
  disabledReason?: string;
  testId?: string;
  className?: string;
}) {
  return (
    <IconButton
      size="sm"
      icon={icon}
      label={label}
      tooltip={disabled && disabledReason ? disabledReason : label}
      aria-disabled={disabled || undefined}
      data-testid={testId}
      onClick={() => {
        if (!disabled) onClick();
      }}
      className={cn(
        "size-7 rounded-lg",
        disabled && "cursor-not-allowed opacity-40 hover:bg-transparent hover:text-cx-fg-3",
        className,
      )}
    />
  );
}

export function CopyMessageButton({ text, label = "复制", disabled }: { text: string; label?: string; disabled?: boolean }) {
  const { copied, copy } = useCopy();
  return (
    <IconButton
      size="sm"
      icon={copied ? "check" : "copy"}
      label={copied ? "已复制" : label}
      aria-disabled={disabled || undefined}
      onClick={() => {
        if (!disabled && text) void copy(text);
      }}
      className={cn("size-7 rounded-lg", copied && "text-cx-success hover:text-cx-success")}
    />
  );
}

export function CopyLinkButton({ messageId }: { messageId: string }) {
  const { copy } = useCopy();
  return (
    <IconButton
      size="sm"
      icon="link"
      label="复制消息链接"
      onClick={() => {
        void copy(messageDeepLink(messageId)).then((ok) => {
          if (ok) toast({ title: "已复制消息链接", tone: "success", icon: "link" });
        });
      }}
      className="size-7 rounded-lg"
    />
  );
}

/**
 * Hover/focus-revealed action row under a message. Touch devices and the
 * newest reply keep it visible (`pinned`).
 */
export function MessageActionBar({
  align = "start",
  pinned = false,
  meta,
  children,
  className,
}: {
  align?: "start" | "end";
  pinned?: boolean;
  meta?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "cx-msg-actions flex h-8 items-center gap-0.5 text-cx-fg-3 transition-opacity duration-150 ease-cx-out",
        align === "end" ? "flex-row-reverse self-end" : "self-start",
        pinned
          ? "opacity-100"
          : "opacity-0 group-hover/msg:opacity-100 group-focus-within/msg:opacity-100 [@media(hover:none)]:opacity-100",
        className,
      )}
      data-pinned={pinned || undefined}
    >
      <div className={cn("flex items-center gap-0.5", align === "end" && "flex-row-reverse")}>{children}</div>
      {meta ? (
        <span className={cn("cx-tabular select-none px-1.5 text-[11.5px] text-cx-fg-4", align === "end" ? "mr-0.5" : "ml-0.5")}>
          {meta}
        </span>
      ) : null}
    </div>
  );
}
