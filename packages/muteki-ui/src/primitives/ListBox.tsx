"use client";

import {
  useCallback,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
} from "react";
import {
  createSelectController,
  Select as UiSelect,
  SelectContent,
  SelectEmpty,
  SelectGroup,
  SelectGroupLabel,
  SelectInput,
  SelectItem,
  SelectList,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import { cn } from "../utils/cn";
import { Icon, type IconName } from "../icons/Icon";
import { splitPlacement, useControllableOpen, type Placement } from "./Popover";
import { ensureImeCompositionTracking, guardOverlayDismiss } from "./_ime";

export interface ListOption<V extends string = string> {
  value: V;
  label: ReactNode;
  /** Plain text searched when filtering options (needed if `label` is JSX). */
  keywords?: string;
  textValue?: string;
  description?: ReactNode;
  icon?: IconName;
  leading?: ReactNode;
  trailing?: ReactNode;
  disabled?: boolean;
  section?: string;
}

function optionText(option: ListOption): string {
  if (typeof option.label === "string") return option.label;
  return option.textValue ?? option.keywords ?? option.value;
}

export function filterOptions<V extends string>(options: ListOption<V>[], query: string): ListOption<V>[] {
  const q = query.trim().toLowerCase();
  if (!q) return options;
  const parts = q.split(/\s+/);
  return options.filter((o) => {
    const hay = `${optionText(o)} ${o.textValue ?? ""} ${typeof o.description === "string" ? o.description : ""} ${o.value}`.toLowerCase();
    return parts.every((p) => hay.includes(p));
  });
}

export function OptionList<V extends string>({
  options,
  value,
  selected,
  activeIndex,
  onActiveIndexChange,
  onSelect,
  emptyText = "没有匹配项",
  className,
  id,
}: {
  options: ListOption<V>[];
  value?: V | null;
  selected?: V | null;
  activeIndex: number;
  onActiveIndexChange: (index: number) => void;
  onSelect: (value: V) => void;
  emptyText?: ReactNode;
  className?: string;
  id?: string;
}) {
  const effectiveValue = selected !== undefined ? selected : value;
  const listRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (activeIndex < 0) return;
    listRef.current?.querySelector<HTMLElement>(`[data-index="${activeIndex}"]`)?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  if (!options.length) {
    return <div className="px-3 py-6 text-center text-cx-xs text-cx-fg-4">{emptyText}</div>;
  }

  let prevSection: string | undefined;
  return (
    <div ref={listRef} id={id} role="listbox" className={cn("cx-scroll flex min-h-0 flex-col overflow-y-auto p-1", className)}>
      {options.map((option, index) => {
        const heading = option.section && option.section !== prevSection ? option.section : null;
        prevSection = option.section;
        const active = index === activeIndex;
        const chosen = option.value === effectiveValue;
        return (
          <div key={option.value} className="flex flex-col">
            {heading ? (
              <div className="px-2 pb-1 pt-2 text-cx-2xs font-medium text-cx-fg-4 first:pt-1">{heading}</div>
            ) : null}
            <div
              id={id ? `${id}-opt-${index}` : undefined}
              role="option"
              aria-selected={chosen}
              aria-disabled={option.disabled || undefined}
              data-index={index}
              data-active={active || undefined}
              data-selected={chosen ? "true" : undefined}
              onPointerMove={() => {
                if (!option.disabled && !active) onActiveIndexChange(index);
              }}
              onPointerDown={(event) => event.preventDefault()}
              onClick={() => {
                if (!option.disabled) onSelect(option.value);
              }}
              className={cn(
                "flex min-h-8 shrink-0 cursor-default select-none items-center gap-2.5 rounded-[8px] px-2 py-1.5 text-cx-sm leading-5 text-cx-fg",
                "data-[active=true]:bg-cx-hover",
                option.disabled && "opacity-40",
              )}
            >
              {option.leading ?? (option.icon ? <Icon name={option.icon} size={15} className="text-cx-fg-3" /> : null)}
              <span className="flex min-w-0 flex-1 flex-col">
                <span className="truncate">{option.label}</span>
                {option.description ? (
                  <span className="truncate text-cx-xs leading-4 text-cx-fg-3">{option.description}</span>
                ) : null}
              </span>
              {option.trailing}
              <Icon name="check" size={14} className={cn("shrink-0 text-cx-fg", chosen ? "opacity-100" : "opacity-0")} />
            </div>
          </div>
        );
      })}
    </div>
  );
}

function nextEnabled<V extends string>(options: ListOption<V>[], from: number, dir: 1 | -1): number {
  if (!options.length) return -1;
  for (let step = 1; step <= options.length; step += 1) {
    const index = (from + dir * step + options.length * 2) % options.length;
    if (!options[index]?.disabled) return index;
  }
  return -1;
}

export function useListKeyboard<V extends string>(
  options: ListOption<V>[],
  onSelect: (value: V) => void,
  initial = 0,
) {
  const [activeIndex, setActiveIndex] = useState(initial);
  useEffect(() => {
    if (activeIndex >= options.length) setActiveIndex(options.length ? 0 : -1);
  }, [activeIndex, options.length]);
  const onKeyDown = (event: React.KeyboardEvent) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActiveIndex((i) => nextEnabled(options, i, 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setActiveIndex((i) => nextEnabled(options, i < 0 ? 0 : i, -1));
    } else if (event.key === "Home") {
      event.preventDefault();
      setActiveIndex(nextEnabled(options, -1, 1));
    } else if (event.key === "End") {
      event.preventDefault();
      setActiveIndex(nextEnabled(options, 0, -1));
    } else if (event.key === "Enter" && activeIndex >= 0 && options[activeIndex] && !options[activeIndex].disabled) {
      event.preventDefault();
      onSelect(options[activeIndex].value);
    }
  };
  return { activeIndex, setActiveIndex, onKeyDown };
}

export interface SelectProps<V extends string = string> {
  id?: string;
  value?: V | null;
  onChange: (value: V) => void;
  options: ListOption<V>[];
  placeholder?: string;
  ariaLabel?: string;
  ariaLabelledBy?: string;
  searchable?: boolean;
  searchPlaceholder?: string;
  emptyText?: ReactNode;
  placement?: Placement;
  disabled?: boolean;
  invalid?: boolean;
  trigger?: ReactElement;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  header?: ReactNode;
  footer?: ReactNode | ((close: () => void) => ReactNode);
  className?: string;
  popoverClassName?: string;
  size?: "xs" | "sm" | "md" | "lg";
  testId?: string;
}

const TRIGGER_SIZE: Record<NonNullable<SelectProps["size"]>, string> = {
  xs: "h-6 px-2 text-cx-xs rounded-[6px]",
  sm: "h-7 px-2.5 text-cx-xs rounded-[7px]",
  md: "h-8 px-3 text-cx-sm rounded-[8px]",
  lg: "h-10 px-3 text-cx-base rounded-[10px]",
};

export function Select<V extends string = string>({
  id,
  value,
  onChange,
  options,
  placeholder = "请选择…",
  ariaLabel,
  ariaLabelledBy,
  searchable,
  searchPlaceholder = "搜索…",
  emptyText = "没有匹配项",
  placement = "bottom-start",
  disabled,
  invalid,
  trigger,
  open: openProp,
  onOpenChange,
  header,
  footer,
  className,
  popoverClassName,
  size = "md",
  testId,
}: SelectProps<V>) {
  const [open, setOpen] = useControllableOpen(openProp, false, onOpenChange);
  const [controller] = useState(() => createSelectController<V, false>());
  const searchInputId = useId();
  const { side, align } = splitPlacement(placement);

  useEffect(() => {
    ensureImeCompositionTracking();
  }, []);

  const current = options.find((o) => o.value === value);

  const optionByValue = useMemo(() => {
    const map = new Map<string, ListOption<V>>();
    for (const o of options) map.set(o.value, o);
    return map;
  }, [options]);

  const sections = useMemo(() => {
    const groups: Array<{ section: string | null; items: ListOption<V>[] }> = [];
    for (const option of options) {
      const sec = option.section ?? null;
      const last = groups[groups.length - 1];
      if (!last || last.section !== sec) {
        groups.push({ section: sec, items: [option] });
      } else {
        last.items.push(option);
      }
    }
    return groups;
  }, [options]);

  const close = useCallback(() => {
    setOpen(false);
    controller.close();
  }, [controller, setOpen]);

  const renderItem = (option: ListOption<V>) => {
    const textLabel = optionText(option);
    return (
      <SelectItem
        key={option.value}
        value={option.value}
        label={textLabel}
        disabled={option.disabled}
        data-cx-option=""
        className={cn(
          "flex min-h-8 shrink-0 cursor-default select-none items-center gap-2.5 rounded-[8px] px-2 py-1.5 text-cx-sm leading-5 text-cx-fg outline-none",
          "data-[highlighted]:bg-cx-hover",
          option.disabled && "opacity-40 pointer-events-none",
        )}
      >
        {option.leading ?? (option.icon ? <Icon name={option.icon} size={15} className="shrink-0 text-cx-fg-3" /> : null)}
        <span className="flex min-w-0 flex-1 flex-col">
          <span className="truncate">{option.label}</span>
          {option.description ? (
            <span className="truncate text-cx-xs leading-4 text-cx-fg-3">{option.description}</span>
          ) : null}
        </span>
        {option.trailing}
        <Icon
          name="check"
          size={14}
          className={cn("shrink-0 text-cx-fg", option.value === value ? "opacity-100" : "opacity-0")}
        />
      </SelectItem>
    );
  };

  const defaultTrigger = (
    <button
      id={id}
      type="button"
      disabled={disabled}
      aria-label={ariaLabel}
      aria-labelledby={ariaLabelledBy}
      data-slot="select-trigger"
      data-invalid={invalid ? "true" : undefined}
      data-testid={testId}
      className={cn(
        "cx-press ui-surface-tactile inline-flex min-w-0 items-center gap-2 border border-cx-border bg-cx-elevated text-left text-cx-fg transition-colors hover:border-cx-border-strong disabled:opacity-50",
        "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
        invalid && "border-cx-danger",
        TRIGGER_SIZE[size],
        className,
      )}
    >
      {current?.icon ? <Icon name={current.icon} size={14} className="shrink-0 text-cx-fg-3" /> : current?.leading}
      <SelectValue className={cn("min-w-0 flex-1 truncate", !current && "text-cx-fg-4")}>
        {() => current?.label ?? placeholder}
      </SelectValue>
      <Icon name="chevronsUpDown" size={13} className="shrink-0 text-cx-fg-4" />
    </button>
  );

  return (
    <UiSelect<V, false>
      value={value ?? undefined}
      disabled={disabled}
      open={openProp !== undefined ? open : undefined}
      controller={controller}
      itemToLabel={(v) => {
        const found = optionByValue.get(String(v));
        return found ? optionText(found) : String(v);
      }}
      filter={(item, query) => {
        const q = query.trim().toLowerCase();
        if (!q) return true;
        const opt = optionByValue.get(item.value);
        const hay = opt
          ? `${optionText(opt)} ${typeof opt.description === "string" ? opt.description : ""} ${opt.value}`.toLowerCase()
          : `${item.label} ${item.value}`.toLowerCase();
        return q.split(/\s+/).every((part) => hay.includes(part));
      }}
      onValueChange={(nextValue) => {
        if (nextValue !== null) onChange(nextValue);
      }}
      onOpenChange={(nextOpen, details) => {
        if (guardOverlayDismiss(nextOpen, details)) return;
        setOpen(nextOpen);
      }}
    >
      <SelectTrigger
        render={trigger ?? defaultTrigger}
        aria-label={trigger ? ariaLabel : undefined}
        data-state={open ? "open" : "closed"}
      />
      <SelectContent
        side={side}
        align={align}
        sideOffset={6}
        data-cx-layer=""
        onClick={(event) => event.stopPropagation()}
        onPointerDown={(event) => event.stopPropagation()}
        style={{
          maxHeight: "min(380px, var(--available-height))",
          transformOrigin: "var(--transform-origin)",
        }}
        className={cn(
          "cx-portal z-(--z-popover) m-0 flex min-w-[180px] max-w-[min(420px,calc(100vw-16px))] flex-col overflow-hidden rounded-[12px] bg-cx-overlay p-0 text-cx-fg shadow-cx-pop outline-none",
          "dark:shadow-[inset_0_1px_0_color-mix(in_srgb,white_8%,transparent),var(--cx-shadow-pop)]",
          !trigger && "min-w-(--anchor-width)",
          "muteki-popover-anim",
          popoverClassName,
        )}
      >
        {header}
        {searchable ? (
          <div className="shrink-0 border-b border-cx-border-subtle p-1.5">
            <SelectInput
              id={searchInputId}
              aria-label={searchPlaceholder}
              placeholder={searchPlaceholder}
              className="h-8 w-full rounded-md bg-transparent px-2 text-cx-sm text-cx-fg outline-none placeholder:text-cx-fg-4"
            />
          </div>
        ) : null}
        <SelectEmpty className="px-3 py-6 text-center text-cx-xs text-cx-fg-4 empty:hidden">
          {emptyText}
        </SelectEmpty>
        <SelectList className="cx-scroll flex min-h-0 flex-auto flex-col overflow-y-auto p-1 empty:hidden">
          {sections.map((group, idx) =>
            group.section ? (
              <SelectGroup key={`${group.section}-${idx}`} className="flex flex-col">
                <SelectGroupLabel className="px-2 pb-1 pt-2 text-cx-2xs font-medium text-cx-fg-4 first:pt-1">
                  {group.section}
                </SelectGroupLabel>
                {group.items.map(renderItem)}
              </SelectGroup>
            ) : (
              group.items.map(renderItem)
            ),
          )}
        </SelectList>
        {footer ? (
          <div className="shrink-0 border-t border-cx-border-subtle p-1">
            {typeof footer === "function" ? footer(close) : footer}
          </div>
        ) : null}
      </SelectContent>
    </UiSelect>
  );
}
