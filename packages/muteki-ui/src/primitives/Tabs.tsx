"use client";

import {
  createContext,
  useCallback,
  useContext,
  useId,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { LayoutGroup, motion } from "motion/react";
import { cn } from "../utils/cn";
import { useReducedMotion } from "../utils/reducedMotion";
import { Icon, type IconName } from "../icons/Icon";
import { SPRING_LAYOUT } from "./motion";

export interface SegmentOption<V extends string> {
  value: V;
  label?: ReactNode;
  icon?: IconName;
  ariaLabel?: string;
  badge?: ReactNode;
  disabled?: boolean;
}

function rovingKeyDown(event: React.KeyboardEvent<HTMLElement>) {
  const items = Array.from(
    event.currentTarget.querySelectorAll<HTMLElement>("[role='tab']:not([disabled]),[role='radio']:not([disabled])"),
  );
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

/** Pill segmented control with a gliding thumb. */
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
  const item = size === "xs" ? "h-5 px-1.5 text-cx-xs" : size === "md" ? "h-7 px-3 text-cx-sm" : "h-6 px-2 text-cx-xs";
  return (
    <LayoutGroup id={group}>
      <div
        role="radiogroup"
        aria-label={ariaLabel}
        data-slot="segmented-control"
        onKeyDown={rovingKeyDown}
        className={cn("inline-flex items-center gap-0.5 rounded-[8px] border border-cx-border-subtle bg-cx-hover p-0.5", height, className)}
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
              data-selected={selected ? "true" : undefined}
              tabIndex={selected ? 0 : -1}
              disabled={option.disabled}
              onClick={() => onChange(option.value)}
              className={cn(
                "relative inline-flex items-center justify-center gap-1.5 rounded-[6px] font-medium transition-colors duration-150 disabled:opacity-40",
                "focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-[var(--cx-focus)]",
                item,
                selected ? "text-cx-fg" : "text-cx-fg-3 hover:text-cx-fg",
              )}
            >
              {selected ? (
                <motion.span
                  layoutId="thumb"
                  layoutDependency={value}
                  transition={reduced ? { duration: 0 } : SPRING_LAYOUT}
                  className="ui-surface-ring absolute inset-0 rounded-[6px] bg-cx-elevated"
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

/** Underline tab bar with a sliding precision rule indicator. */
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
      <div ref={ref} role="tablist" aria-label={ariaLabel} data-slot="tab-bar" onKeyDown={rovingKeyDown} className={cn("flex items-center gap-1 border-b border-cx-border-subtle", className)}>
        {items.map((item) => {
          const selected = item.value === value;
          return (
            <button
              key={item.value}
              type="button"
              role="tab"
              aria-selected={selected}
              data-selected={selected ? "true" : undefined}
              tabIndex={selected ? 0 : -1}
              onClick={() => onChange(item.value)}
              className={cn(
                "relative inline-flex h-9 items-center gap-1.5 px-2.5 text-cx-sm font-medium transition-colors outline-none",
                "focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--cx-focus)] rounded-[6px]",
                selected ? "text-cx-fg" : "text-cx-fg-3 hover:text-cx-fg",
              )}
            >
              {item.icon ? <Icon name={item.icon} size={14} /> : null}
              {item.label}
              {item.count !== undefined ? <span className="cx-tabular rounded-full bg-cx-hover px-1.5 text-cx-2xs text-cx-fg-3">{item.count}</span> : null}
              {item.alert ? <span className="size-1.5 rounded-full bg-cx-danger" /> : null}
              {selected ? (
                <motion.span layoutId="underline" layoutDependency={value} transition={reduced ? { duration: 0 } : SPRING_LAYOUT} className="absolute inset-x-1.5 -bottom-px h-[2px] rounded-full bg-cx-fg" />
              ) : null}
            </button>
          );
        })}
      </div>
    </LayoutGroup>
  );
}


interface TabsCtx {
  baseId: string;
  value: string;
  onChange: (value: string) => void;
  variant: "underline" | "pill" | "segmented";
  size: "sm" | "md";
}

const TabsContext = createContext<TabsCtx | null>(null);

export interface TabsProps<V extends string = string> {
  value?: V;
  defaultValue?: V;
  onChange?: (value: V) => void;
  variant?: "underline" | "pill" | "segmented";
  size?: "sm" | "md";
  className?: string;
  children: ReactNode;
}

export function Tabs<V extends string = string>({
  value: valueProp,
  defaultValue = "" as V,
  onChange,
  variant = "underline",
  size = "md",
  className,
  children,
}: TabsProps<V>) {
  const [inner, setInner] = useState<string>(defaultValue);
  const controlled = valueProp !== undefined;
  const value = controlled ? valueProp : inner;
  const baseId = useId();

  const handleChange = useCallback(
    (next: string) => {
      if (!controlled) setInner(next);
      onChange?.(next as V);
    },
    [controlled, onChange],
  );

  return (
    <TabsContext.Provider value={{ baseId, value, onChange: handleChange, variant, size }}>
      <LayoutGroup id={baseId}>
        <div data-slot="tabs" className={cn("flex flex-col", className)}>
          {children}
        </div>
      </LayoutGroup>
    </TabsContext.Provider>
  );
}

export function TabsList({
  children,
  className,
  ariaLabel,
}: {
  children: ReactNode;
  className?: string;
  ariaLabel?: string;
}) {
  const ctx = useContext(TabsContext);
  const variant = ctx?.variant ?? "underline";
  return (
    <div
      role="tablist"
      aria-label={ariaLabel}
      data-slot="tabs-list"
      data-variant={variant}
      onKeyDown={rovingKeyDown}
      className={cn(
        "inline-flex items-center gap-1",
        variant === "underline" && "border-b border-cx-border-subtle",
        variant === "segmented" && "rounded-[8px] border border-cx-border-subtle bg-cx-hover p-0.5",
        variant === "pill" && "gap-1.5",
        className,
      )}
    >
      {children}
    </div>
  );
}

export function TabsTrigger({
  value,
  disabled,
  icon,
  count,
  children,
  className,
  testId,
}: {
  value: string;
  disabled?: boolean;
  icon?: IconName;
  count?: number;
  children: ReactNode;
  className?: string;
  testId?: string;
}) {
  const ctx = useContext(TabsContext);
  const reduced = useReducedMotion();
  if (!ctx) return null;
  const selected = ctx.value === value;
  const isSm = ctx.size === "sm";

  return (
    <button
      type="button"
      role="tab"
      id={`${ctx.baseId}-tab-${value}`}
      aria-controls={`${ctx.baseId}-panel-${value}`}
      aria-selected={selected}
      disabled={disabled}
      tabIndex={selected ? 0 : -1}
      data-slot="tabs-trigger"
      data-selected={selected ? "true" : undefined}
      data-disabled={disabled ? "true" : undefined}
      data-testid={testId}
      onClick={() => !disabled && ctx.onChange(value)}
      className={cn(
        "relative inline-flex shrink-0 items-center justify-center gap-1.5 font-medium whitespace-nowrap outline-none transition-colors disabled:pointer-events-none disabled:opacity-45",
        "focus-visible:outline-2 focus-visible:outline-offset-[-1px] focus-visible:outline-[var(--cx-focus)]",
        ctx.variant === "underline" && (isSm ? "h-8 px-2.5 text-cx-xs rounded-[6px]" : "h-9 px-3 text-cx-sm rounded-[6px]"),
        ctx.variant === "segmented" && (isSm ? "h-6 px-2 text-cx-xs rounded-[6px]" : "h-7 px-3 text-cx-sm rounded-[6px]"),
        ctx.variant === "pill" && (isSm ? "h-7 px-2.5 text-cx-xs rounded-full" : "h-8 px-3 text-cx-sm rounded-full"),
        ctx.variant === "pill" && (selected ? "bg-cx-elevated text-cx-fg ring-1 ring-cx-ring shadow-cx-xs" : "text-cx-fg-3 hover:text-cx-fg hover:bg-cx-hover"),
        ctx.variant !== "pill" && (selected ? "text-cx-fg" : "text-cx-fg-3 hover:text-cx-fg"),
        className,
      )}
    >
      {ctx.variant === "segmented" && selected ? (
        <motion.span
          data-slot="tabs-indicator"
          layoutId="tab-seg-pill"
          layoutDependency={ctx.value}
          transition={reduced ? { duration: 0 } : SPRING_LAYOUT}
          className="ui-surface-ring absolute inset-0 rounded-[6px] bg-cx-elevated"
        />
      ) : null}
      <span data-slot="tabs-label" className="relative inline-flex items-center gap-1.5">
        {icon ? <Icon name={icon} size={isSm ? 13 : 14} /> : null}
        {children}
        {count !== undefined ? <span className="cx-tabular rounded-full bg-cx-hover px-1.5 text-cx-2xs text-cx-fg-3">{count}</span> : null}
      </span>
      {ctx.variant === "underline" && selected ? (
        <motion.span data-slot="tabs-indicator" layoutId="tab-underline" layoutDependency={ctx.value} transition={reduced ? { duration: 0 } : SPRING_LAYOUT} className="absolute inset-x-1.5 -bottom-px h-[2px] rounded-full bg-cx-fg" />
      ) : null}
    </button>
  );
}

export function TabsContent({
  value,
  keepMounted,
  children,
  className,
}: {
  value: string;
  keepMounted?: boolean;
  children: ReactNode;
  className?: string;
}) {
  const ctx = useContext(TabsContext);
  if (!ctx) return null;
  const active = ctx.value === value;
  if (!active && !keepMounted) return null;
  return (
    <div
      role="tabpanel"
      id={`${ctx.baseId}-panel-${value}`}
      aria-labelledby={`${ctx.baseId}-tab-${value}`}
      hidden={!active}
      data-slot="tabs-content"
      data-selected={active ? "true" : undefined}
      className={cn("outline-none", className)}
    >
      {children}
    </div>
  );
}
