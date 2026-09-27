"use client";

import { useId, useRef, type ReactNode } from "react";
import { LayoutGroup, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { SPRING_LAYOUT, useReducedMotion } from "./motion";

export interface SegmentOption<V extends string> {
  value: V;
  label?: ReactNode;
  icon?: IconName;
  ariaLabel?: string;
  badge?: ReactNode;
  disabled?: boolean;
}

function rovingKeyDown(event: React.KeyboardEvent<HTMLElement>) {
  const items = Array.from(event.currentTarget.querySelectorAll<HTMLElement>("[role='tab']:not([disabled]),[role='radio']:not([disabled])"));
  const index = items.indexOf(document.activeElement as HTMLElement);
  if (index < 0) return;
  let next = -1;
  if (event.key === "ArrowRight" || event.key === "ArrowDown") next = (index + 1) % items.length;
  else if (event.key === "ArrowLeft" || event.key === "ArrowUp") next = (index - 1 + items.length) % items.length;
  else if (event.key === "Home") next = 0;
  else if (event.key === "End") next = items.length - 1;
  if (next < 0) return;
  event.preventDefault();
  items[next].focus();
  items[next].click();
}

/** Pill segmented control with a gliding thumb (unified/split, scope toggles). */
export function SegmentedControl<V extends string>({
  value,
  onChange,
  options,
  size = "sm",
  className,
  ariaLabel,
}: {
  value: V;
  onChange: (value: V) => void;
  options: SegmentOption<V>[];
  size?: "xs" | "sm" | "md";
  className?: string;
  ariaLabel?: string;
}) {
  const group = useId();
  const reduced = useReducedMotion();
  const height = size === "xs" ? "h-6" : size === "md" ? "h-8" : "h-7";
  const item = size === "xs" ? "h-5 px-1.5 text-[11.5px]" : size === "md" ? "h-7 px-3 text-[13px]" : "h-6 px-2 text-[12px]";
  return (
    <LayoutGroup id={group}>
      <div
        role="radiogroup"
        aria-label={ariaLabel}
        onKeyDown={rovingKeyDown}
        className={cn("inline-flex items-center gap-0.5 rounded-lg bg-cx-hover p-0.5", height, className)}
      >
        {options.map((option) => {
          const selected = option.value === value;
          return (
            <button
              key={option.value}
              type="button"
              role="radio"
              aria-checked={selected}
              aria-label={option.ariaLabel}
              tabIndex={selected ? 0 : -1}
              disabled={option.disabled}
              onClick={() => onChange(option.value)}
              className={cn(
                "relative inline-flex items-center justify-center gap-1.5 rounded-md font-medium transition-colors duration-150 disabled:opacity-40",
                item,
                selected ? "text-cx-fg" : "text-cx-fg-3 hover:text-cx-fg",
              )}
            >
              {selected ? (
                <motion.span
                  layoutId="thumb"
                  transition={reduced ? { duration: 0 } : SPRING_LAYOUT}
                  className="absolute inset-0 rounded-md bg-cx-elevated shadow-[0_1px_2px_hsl(var(--cx-shadow-color)/0.12),0_0_0_1px_var(--cx-border-subtle)]"
                />
              ) : null}
              <span className="relative inline-flex items-center gap-1.5">
                {option.icon ? <Icon name={option.icon} size={13} /> : null}
                {option.label}
                {option.badge}
              </span>
            </button>
          );
        })}
      </div>
    </LayoutGroup>
  );
}

export interface TabItem<V extends string> {
  value: V;
  label: ReactNode;
  icon?: IconName;
  count?: number;
  alert?: boolean;
}

/** Underline tab bar with a sliding indicator. */
export function TabBar<V extends string>({
  value,
  onChange,
  items,
  className,
  ariaLabel,
}: {
  value: V;
  onChange: (value: V) => void;
  items: TabItem<V>[];
  className?: string;
  ariaLabel?: string;
}) {
  const group = useId();
  const reduced = useReducedMotion();
  const ref = useRef<HTMLDivElement | null>(null);
  return (
    <LayoutGroup id={group}>
      <div ref={ref} role="tablist" aria-label={ariaLabel} onKeyDown={rovingKeyDown} className={cn("flex items-center gap-1 border-b border-cx-border-subtle", className)}>
        {items.map((item) => {
          const selected = item.value === value;
          return (
            <button
              key={item.value}
              type="button"
              role="tab"
              aria-selected={selected}
              tabIndex={selected ? 0 : -1}
              onClick={() => onChange(item.value)}
              className={cn(
                "relative inline-flex h-9 items-center gap-1.5 px-2.5 text-[13px] font-medium transition-colors",
                selected ? "text-cx-fg" : "text-cx-fg-3 hover:text-cx-fg",
              )}
            >
              {item.icon ? <Icon name={item.icon} size={14} /> : null}
              {item.label}
              {item.count !== undefined ? <span className="cx-tabular rounded-full bg-cx-hover px-1.5 text-[11px] text-cx-fg-3">{item.count}</span> : null}
              {item.alert ? <span className="size-1.5 rounded-full bg-cx-danger" /> : null}
              {selected ? (
                <motion.span layoutId="underline" transition={reduced ? { duration: 0 } : SPRING_LAYOUT} className="absolute inset-x-1.5 -bottom-px h-[2px] rounded-full bg-cx-fg" />
              ) : null}
            </button>
          );
        })}
      </div>
    </LayoutGroup>
  );
}
