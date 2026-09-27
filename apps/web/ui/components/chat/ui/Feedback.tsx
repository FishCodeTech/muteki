"use client";

import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { TextShimmer } from "@/components/agentui/motion/text-shimmer";

export function Spinner({ size = 14, className }: { size?: number; className?: string }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      className={cn("cx-spin shrink-0 text-current", className)}
      aria-hidden
    >
      <circle cx="8" cy="8" r="6.25" fill="none" stroke="currentColor" strokeOpacity="0.18" strokeWidth="1.75" />
      <path d="M14.25 8A6.25 6.25 0 0 0 8 1.75" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" />
    </svg>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={cn("cx-skeleton h-3", className)} />;
}

export function ShimmerText({ children, className, duration = 1.8 }: { children: ReactNode; className?: string; duration?: number }) {
  return <TextShimmer duration={duration} className={className}>{children}</TextShimmer>;
}

export type Tone = "neutral" | "accent" | "success" | "warning" | "danger" | "running";

const DOT: Record<Tone, string> = {
  neutral: "bg-cx-fg-4",
  accent: "bg-cx-accent",
  success: "bg-cx-success",
  warning: "bg-cx-warning",
  danger: "bg-cx-danger",
  running: "bg-cx-accent cx-pulse-dot",
};

export function StatusDot({ tone = "neutral", className, label }: { tone?: Tone; className?: string; label?: string }) {
  return <span role={label ? "img" : undefined} aria-label={label} className={cn("inline-block size-1.5 shrink-0 rounded-full", DOT[tone], className)} />;
}

const BADGE: Record<Tone, string> = {
  neutral: "bg-cx-hover text-cx-fg-2",
  accent: "bg-cx-accent-soft text-cx-accent",
  success: "bg-cx-success-soft text-cx-success",
  warning: "bg-cx-warning-soft text-cx-warning",
  danger: "bg-cx-danger-soft text-cx-danger",
  running: "bg-cx-accent-soft text-cx-accent",
};

export function Badge({
  tone = "neutral",
  children,
  className,
  icon,
  dot,
}: {
  tone?: Tone;
  children: ReactNode;
  className?: string;
  icon?: IconName;
  dot?: boolean;
}) {
  return (
    <span className={cn("inline-flex h-5 shrink-0 items-center gap-1 rounded-md px-1.5 text-[11.5px] font-medium leading-none", BADGE[tone], className)}>
      {dot ? <StatusDot tone={tone} /> : null}
      {icon ? <Icon name={icon} size={12} /> : null}
      {children}
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
    <span className={cn("cx-tabular inline-flex items-center gap-1.5 font-cx-mono text-[11.5px] leading-none", className)}>
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
    <div className={cn("flex flex-col items-center justify-center text-center", isCompact ? "gap-2 px-4 py-6" : "gap-3 px-6 py-12", className)}>
      {icon ? (
        <span className={cn("grid place-items-center rounded-2xl bg-cx-hover text-cx-fg-3", isCompact ? "size-9" : "size-11")}>
          <Icon name={icon} size={isCompact ? 17 : 20} />
        </span>
      ) : null}
      <div className="flex max-w-[320px] flex-col gap-1">
        <p className="text-[13.5px] font-medium text-cx-fg">{title}</p>
        {description ? <p className="text-[12.5px] leading-5 text-cx-fg-3">{description}</p> : null}
      </div>
      {action ? <div className="mt-1 flex items-center gap-2">{action}</div> : null}
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
  tone?: Tone;
  icon?: IconName;
  title?: ReactNode;
  children?: ReactNode;
  action?: ReactNode;
  onDismiss?: () => void;
  className?: string;
  testId?: string;
  role?: "alert" | "status" | "note";
}) {
  const tint: Record<Tone, string> = {
    neutral: "border-cx-border bg-cx-bg-subtle text-cx-fg-2",
    accent: "border-cx-accent-line/40 bg-cx-accent-soft text-cx-fg",
    success: "border-[color-mix(in_srgb,var(--green)_30%,transparent)] bg-cx-success-soft text-cx-fg",
    warning: "border-[color-mix(in_srgb,var(--amber)_32%,transparent)] bg-cx-warning-soft text-cx-fg",
    danger: "border-[color-mix(in_srgb,var(--red)_30%,transparent)] bg-cx-danger-soft text-cx-fg",
    running: "border-cx-accent-line/40 bg-cx-accent-soft text-cx-fg",
  };
  const iconTone: Record<Tone, string> = {
    neutral: "text-cx-fg-3",
    accent: "text-cx-accent",
    success: "text-cx-success",
    warning: "text-cx-warning",
    danger: "text-cx-danger",
    running: "text-cx-accent",
  };
  const defaultIcon: Record<Tone, IconName> = {
    neutral: "info",
    accent: "info",
    success: "checkCircle",
    warning: "alert",
    danger: "circleAlert",
    running: "loader",
  };
  return (
    <div role={role} data-testid={testId} className={cn("flex items-start gap-2.5 rounded-xl border px-3 py-2.5 text-[13px] leading-5", tint[tone], className)}>
      <Icon name={icon ?? defaultIcon[tone]} size={15} className={cn("mt-[2px] shrink-0", iconTone[tone], tone === "running" && "cx-spin")} />
      <div className="min-w-0 flex-1">
        {title ? <p className="font-medium text-cx-fg">{title}</p> : null}
        {children ? <div className={cn(title && "mt-0.5 text-cx-fg-2")}>{children}</div> : null}
      </div>
      {action ? <div className="flex shrink-0 items-center gap-1">{action}</div> : null}
      {onDismiss ? (
        <button type="button" aria-label="关闭" onClick={onDismiss} className="-mr-1 grid size-6 shrink-0 place-items-center rounded-md text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg">
          <Icon name="x" size={13} />
        </button>
      ) : null}
    </div>
  );
}
