"use client";

import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { CopyButton, type Tone } from "@/components/chat/ui";

/** Quiet section heading used inside sheets and dialogs. */
export function DialogSection({
  title,
  icon,
  meta,
  actions,
  children,
  className,
  testId,
}: {
  title: ReactNode;
  icon?: IconName;
  meta?: ReactNode;
  actions?: ReactNode;
  children?: ReactNode;
  className?: string;
  testId?: string;
}) {
  return (
    <section className={cn("cx-dialog-section", className)} data-testid={testId}>
      <header className="flex min-h-7 items-center gap-2">
        {icon ? <Icon name={icon} size={14} className="shrink-0 text-cx-fg-4" /> : null}
        <h3 className="min-w-0 flex-1 truncate text-[13px] font-semibold text-cx-fg-2">{title}</h3>
        {meta ? <span className="shrink-0 text-[12px] text-cx-fg-4">{meta}</span> : null}
        {actions ? <div className="flex shrink-0 items-center gap-0.5">{actions}</div> : null}
      </header>
      {children ? <div className="mt-1.5">{children}</div> : null}
    </section>
  );
}

/** Two-column key/value list with hairline row separators. */
export function MetaList({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <dl className={cn("overflow-hidden rounded-xl border border-cx-border-subtle bg-cx-elevated", className)}>
      {children}
    </dl>
  );
}

export function MetaRow({
  label,
  children,
  mono,
  copy,
  title,
}: {
  label: ReactNode;
  children: ReactNode;
  mono?: boolean;
  /** When set, a copy affordance appears on row hover. */
  copy?: string;
  title?: string;
}) {
  return (
    <div className="group/meta flex min-h-9 items-center gap-3 border-b border-cx-border-subtle px-3 py-1.5 last:border-b-0">
      <dt className="w-[88px] shrink-0 text-[13px] text-cx-fg-3">{label}</dt>
      <dd
        className={cn(
          "min-w-0 flex-1 truncate text-[13px] text-cx-fg",
          mono && "font-cx-mono text-[12px] text-cx-fg-2",
        )}
        title={title ?? (typeof children === "string" ? children : undefined)}
      >
        {children}
      </dd>
      {copy ? (
        <span className="-mr-1 shrink-0 opacity-0 transition-opacity group-hover/meta:opacity-100 focus-within:opacity-100">
          <CopyButton text={copy} label="复制" />
        </span>
      ) : null}
    </div>
  );
}

/** Large tabular metric used in overview grids. */
export function MetricTile({
  label,
  value,
  hint,
  tone = "neutral",
}: {
  label: ReactNode;
  value: ReactNode;
  hint?: ReactNode;
  tone?: Tone;
}) {
  const valueTone: Record<Tone, string> = {
    neutral: "text-cx-fg",
    accent: "text-cx-accent",
    success: "text-cx-success",
    warning: "text-cx-warning",
    danger: "text-cx-danger",
    running: "text-cx-accent",
  };
  return (
    <div className="flex min-w-0 flex-col gap-1 rounded-xl border border-cx-border-subtle bg-cx-elevated px-3 py-2.5">
      <span className="truncate text-[12px] text-cx-fg-3">{label}</span>
      <span className={cn("cx-tabular truncate text-[18px] font-semibold leading-6 tracking-[-0.01em]", valueTone[tone])}>{value}</span>
      {hint ? <span className="truncate text-[12px] text-cx-fg-4">{hint}</span> : null}
    </div>
  );
}

/** Tinted square that carries an entity icon (tool / diff / artifact / error). */
export function IconTile({ icon, tone = "neutral", size = "md" }: { icon: IconName; tone?: Tone; size?: "sm" | "md" }) {
  const tint: Record<Tone, string> = {
    neutral: "bg-cx-hover text-cx-fg-2",
    accent: "bg-cx-accent-soft text-cx-accent",
    success: "bg-cx-success-soft text-cx-success",
    warning: "bg-cx-warning-soft text-cx-warning",
    danger: "bg-cx-danger-soft text-cx-danger",
    running: "bg-cx-accent-soft text-cx-accent",
  };
  return (
    <span className={cn("grid shrink-0 place-items-center rounded-[10px]", size === "sm" ? "size-7" : "size-8", tint[tone])}>
      <Icon name={icon} size={size === "sm" ? 14 : 16} />
    </span>
  );
}
