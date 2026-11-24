"use client";

import type { CSSProperties, ElementType, ReactNode } from "react";
import { cn } from "../utils/cn";
import { Icon, type IconName } from "../icons/Icon";

export function Spinner({ size = 14, className }: { size?: number; className?: string }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      data-slot="spinner"
      className={cn("cx-spin shrink-0 text-current", className)}
      aria-hidden
    >
      <circle cx="8" cy="8" r="6.25" fill="none" stroke="currentColor" strokeOpacity="0.18" strokeWidth="1.75" />
      <path d="M14.25 8A6.25 6.25 0 0 0 8 1.75" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" />
    </svg>
  );
}

export function Skeleton({ className, style }: { className?: string; style?: CSSProperties }) {
  return <div data-slot="skeleton" style={style} className={cn("cx-skeleton h-3 rounded-[6px]", className)} />;
}

export function ShimmerText({
  children,
  as: Comp = "span",
  className,
  duration = 2.0,
}: {
  children: ReactNode;
  as?: ElementType;
  className?: string;
  duration?: number;
}) {
  return (
    <Comp
      data-slot="shimmer-text"
      style={{ animationDuration: `${duration}s` }}
      className={cn("muteki-text-shimmer inline-block", className)}
    >
      {children}
    </Comp>
  );
}

export type Tone = "neutral" | "accent" | "success" | "warning" | "danger" | "running";
export type AlertTone = Tone | "info";

const DOT: Record<AlertTone, string> = {
  neutral: "bg-cx-fg-4",
  accent: "bg-cx-accent",
  info: "bg-[var(--ui-info)]",
  success: "bg-cx-success",
  warning: "bg-cx-warning",
  danger: "bg-cx-danger",
  running: "bg-cx-accent cx-pulse-dot",
};

export function StatusDot({ tone = "neutral", className, label }: { tone?: AlertTone; className?: string; label?: string }) {
  return <span data-slot="status-dot" role={label ? "img" : undefined} aria-label={label} className={cn("inline-block size-1.5 shrink-0 rounded-full", DOT[tone], className)} />;
}

const BADGE_SOFT: Record<AlertTone, string> = {
  neutral: "bg-cx-hover text-cx-fg-2 border-transparent",
  accent: "bg-cx-accent-soft text-cx-accent border-transparent",
  info: "bg-[color-mix(in_srgb,var(--ui-info)_14%,transparent)] text-[var(--ui-info)] border-transparent",
  success: "bg-cx-success-soft text-cx-success border-transparent",
  warning: "bg-cx-warning-soft text-cx-warning border-transparent",
  danger: "bg-cx-danger-soft text-cx-danger border-transparent",
  running: "bg-cx-accent-soft text-cx-accent border-transparent",
};

const BADGE_OUTLINE: Record<AlertTone, string> = {
  neutral: "border-cx-border text-cx-fg-2 bg-cx-elevated",
  accent: "border-cx-accent-line/60 text-cx-accent bg-cx-accent-soft/60",
  info: "border-[color-mix(in_srgb,var(--ui-info)_35%,transparent)] text-[var(--ui-info)] bg-transparent",
  success: "border-[color-mix(in_srgb,var(--green)_35%,transparent)] text-cx-success bg-transparent",
  warning: "border-[color-mix(in_srgb,var(--amber)_35%,transparent)] text-cx-warning bg-transparent",
  danger: "border-[color-mix(in_srgb,var(--red)_35%,transparent)] text-cx-danger bg-transparent",
  running: "border-cx-accent-line/60 text-cx-accent bg-cx-accent-soft/60",
};

export function Badge({
  tone = "neutral",
  variant = "soft",
  children,
  className,
  icon,
  dot,
}: {
  tone?: AlertTone;
  variant?: "soft" | "outline";
  children: ReactNode;
  className?: string;
  icon?: IconName;
  dot?: boolean;
}) {
  return (
    <span
      data-slot="badge"
      data-tone={tone}
      className={cn(
        "inline-flex h-5 shrink-0 items-center gap-1 rounded-[6px] border px-1.5 text-cx-xs font-medium leading-none",
        variant === "outline" ? BADGE_OUTLINE[tone] : BADGE_SOFT[tone],
        className,
      )}
    >
      {dot ? <StatusDot tone={tone} /> : null}
      {icon ? <Icon name={icon} size={12} /> : null}
      {children}
    </span>
  );
}

export function Chip({
  tone = "neutral",
  variant = "soft",
  size = "sm",
  selected,
  disabled,
  icon,
  dot,
  onClick,
  onRemove,
  children,
  className,
  style,
}: {
  tone?: AlertTone;
  variant?: "soft" | "outline";
  size?: "xs" | "sm" | "md";
  selected?: boolean;
  disabled?: boolean;
  icon?: IconName;
  dot?: boolean;
  onClick?: () => void;
  onRemove?: () => void;
  children: ReactNode;
  className?: string;
  style?: CSSProperties;
}) {
  const height = size === "xs" ? "h-5 px-1.5 text-cx-2xs" : size === "md" ? "h-7 px-2.5 text-cx-xs" : "h-6 px-2 text-cx-xs";
  const classes = cn(
    "inline-flex shrink-0 items-center gap-1.5 rounded-full border font-medium leading-none transition-colors",
    height,
    variant === "outline" ? BADGE_OUTLINE[tone] : BADGE_SOFT[tone],
    selected && "border-cx-accent bg-cx-accent-soft text-cx-accent",
    onClick && "cx-press cursor-pointer hover:border-cx-border-strong",
    disabled && "pointer-events-none opacity-50",
    className,
  );
  const content = (
    <>
      {dot ? <StatusDot tone={tone} /> : null}
      {icon ? <Icon name={icon} size={size === "xs" ? 11 : 12} /> : null}
      <span className="truncate">{children}</span>
      {onRemove ? (
        <button
          type="button"
          aria-label="移除"
          onClick={(e) => { e.stopPropagation(); onRemove(); }}
          className="-mr-0.5 grid size-3.5 place-items-center rounded-full hover:bg-black/15"
        >
          <Icon name="x" size={10} />
        </button>
      ) : null}
    </>
  );
  if (onClick) {
    return (
      <button
        type="button"
        disabled={disabled}
        data-slot="chip"
        data-selected={selected ? "true" : undefined}
        onClick={onClick}
        className={classes}
        style={style}
      >
        {content}
      </button>
    );
  }
  return (
    <span data-slot="chip" data-selected={selected ? "true" : undefined} className={classes} style={style}>
      {content}
    </span>
  );
}

function compact(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1).replace(/\.0$/, "")}m`;
  if (value >= 10_000) return `${Math.round(value / 1000)}k`;
  if (value >= 1_000) return `${(value / 1000).toFixed(1).replace(/\.0$/, "")}k`;
  return String(value);
}

export function DiffStat({ additions, deletions, className, bar }: { additions: number; deletions: number; className?: string; bar?: boolean }) {
  const total = additions + deletions;
  const blocks = 5;
  const addBlocks = total ? Math.round((additions / total) * blocks) : 0;
  return (
    <span data-slot="diff-stat" className={cn("cx-tabular inline-flex items-center gap-1.5 font-cx-mono text-cx-xs leading-none", className)}>
      <span className="text-cx-add">+{compact(additions)}</span>
      <span className="text-cx-del">−{compact(deletions)}</span>
      {bar && total ? (
        <span className="inline-flex gap-[2px]" aria-hidden>
          {Array.from({ length: blocks }, (_, i) => (
            <span key={i} className={cn("size-[7px] rounded-[2px]", i < addBlocks ? "bg-cx-add" : "bg-cx-del")} />
          ))}
        </span>
      ) : null}
    </span>
  );
}

export function EmptyState({
  icon,
  title,
  description,
  action,
  className,
  compact: isCompact,
}: {
  icon?: IconName;
  title: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
  className?: string;
  compact?: boolean;
}) {
  return (
    <div data-slot="empty-state" className={cn("flex flex-col items-center justify-center text-center", isCompact ? "gap-2 px-4 py-6" : "gap-3 px-6 py-12", className)}>
      {icon ? (
        <span className={cn("grid place-items-center border border-cx-border bg-cx-hover text-cx-fg-3", isCompact ? "size-9 rounded-xl" : "size-11 rounded-2xl")}>
          <Icon name={icon} size={isCompact ? 17 : 20} />
        </span>
      ) : null}
      <div className="flex max-w-[320px] flex-col gap-1">
        <p className="text-cx-sm font-medium text-cx-fg">{title}</p>
        {description ? <p className="text-cx-xs leading-5 text-cx-fg-3">{description}</p> : null}
      </div>
      {action ? <div className="mt-1 flex items-center gap-2">{action}</div> : null}
    </div>
  );
}

const INLINE_ICON: Record<AlertTone, string> = {
  neutral: "text-cx-fg-3",
  accent: "text-cx-accent",
  info: "text-[var(--ui-info)]",
  success: "text-cx-success",
  warning: "text-cx-warning",
  danger: "text-cx-danger",
  running: "text-cx-fg-3",
};

export function InlineStatus({
  tone = "neutral",
  icon,
  children,
  action,
  className,
  role = "status",
}: {
  tone?: AlertTone;
  icon?: IconName;
  children: ReactNode;
  action?: ReactNode;
  className?: string;
  role?: "status" | "alert";
}) {
  const glyph = icon ?? (tone === "running" ? undefined : tone === "danger" ? "circleAlert" : tone === "warning" ? "alert" : undefined);
  return (
    <div
      role={role}
      data-slot="inline-status"
      data-tone={tone}
      className={cn("inline-flex min-w-0 items-center gap-2 text-cx-xs leading-5 text-cx-fg-2", className)}
    >
      {tone === "running" ? <Spinner size={13} className="text-cx-fg-3" /> : glyph ? (
        <span className={cn("inline-flex shrink-0", INLINE_ICON[tone])} aria-hidden>
          <Icon name={glyph} size={14} />
        </span>
      ) : null}
      <span className="min-w-0">{children}</span>
      {action ? (
        <span className="shrink-0 text-cx-fg-3 [&_a]:underline [&_a]:decoration-cx-border-strong [&_a]:underline-offset-4 [&_a]:transition-colors [&_a:hover]:text-cx-fg [&_a:hover]:decoration-current">
          {action}
        </span>
      ) : null}
    </div>
  );
}

export function Callout({
  tone = "neutral",
  icon,
  title,
  children,
  action,
  onDismiss,
  className,
  testId,
  role,
}: {
  tone?: AlertTone;
  icon?: IconName;
  title?: ReactNode;
  children?: ReactNode;
  action?: ReactNode;
  onDismiss?: () => void;
  className?: string;
  testId?: string;
  role?: "alert" | "status" | "note";
}) {
  const tint: Record<AlertTone, string> = {
    neutral: "bg-cx-bg-subtle text-cx-fg-2",
    accent: "[--ui-surface-ring:color-mix(in_srgb,var(--cx-accent-line)_40%,transparent)] bg-cx-accent-soft text-cx-fg",
    info: "[--ui-surface-ring:color-mix(in_srgb,var(--ui-info)_32%,transparent)] bg-[color-mix(in_srgb,var(--ui-info)_10%,transparent)] text-cx-fg",
    success: "[--ui-surface-ring:color-mix(in_srgb,var(--green)_30%,transparent)] bg-cx-success-soft text-cx-fg",
    warning: "bg-cx-elevated text-cx-fg",
    danger: "[--ui-surface-ring:color-mix(in_srgb,var(--red)_30%,transparent)] bg-cx-danger-soft text-cx-fg",
    running: "[--ui-surface-ring:color-mix(in_srgb,var(--cx-accent-line)_40%,transparent)] bg-cx-accent-soft text-cx-fg",
  };
  const iconTone: Record<AlertTone, string> = {
    neutral: "text-cx-fg-3",
    accent: "text-cx-accent",
    info: "text-[var(--ui-info)]",
    success: "text-cx-success",
    warning: "text-cx-warning",
    danger: "text-cx-danger",
    running: "text-cx-accent",
  };
  const defaultIcon: Record<AlertTone, IconName> = {
    neutral: "info",
    accent: "info",
    info: "info",
    success: "checkCircle",
    warning: "alert",
    danger: "circleAlert",
    running: "loader",
  };
  return (
    <div
      role={role}
      data-slot="alert"
      data-tone={tone}
      data-testid={testId}
      className={cn("ui-surface-ring flex items-start gap-2.5 rounded-[10px] px-3.5 py-2.5 text-cx-sm leading-5", tint[tone], className)}
    >
      <Icon name={icon ?? defaultIcon[tone]} size={15} className={cn("mt-[2px] shrink-0", iconTone[tone], tone === "running" && "cx-spin")} />
      <div className="min-w-0 flex-1">
        {title ? <p data-slot="alert-title" className="font-medium text-cx-fg">{title}</p> : null}
        {children ? <div data-slot="alert-description" className={cn(title && "mt-0.5 text-cx-xs leading-5 text-cx-fg-2")}>{children}</div> : null}
      </div>
      {action ? <div className="flex shrink-0 items-center gap-1">{action}</div> : null}
      {onDismiss ? (
        <button type="button" aria-label="关闭提示" title="关闭提示" onClick={onDismiss} className="cx-press -mr-1 grid size-6 shrink-0 place-items-center self-start rounded-md text-cx-fg-3 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]">
          <Icon name="x" size={13} />
        </button>
      ) : null}
    </div>
  );
}

export { Callout as Alert };
