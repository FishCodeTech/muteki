"use client";

import {
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
} from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Popover } from "./Popover";
import type { Placement } from "./floating";

export interface ListOption<V extends string = string> {
  value: V;
  label: ReactNode;
  /** Plain text used for filtering and type-ahead when `label` is not a string. */
  textValue?: string;
  description?: ReactNode;
  icon?: IconName;
  leading?: ReactNode;
  trailing?: ReactNode;
  disabled?: boolean;
  section?: string;
}

function optionText(option: ListOption): string {
  if (option.textValue) return option.textValue;
  return typeof option.label === "string" ? option.label : option.value;
}

export function filterOptions<V extends string>(options: ListOption<V>[], query: string): ListOption<V>[] {
  const q = query.trim().toLowerCase();
  if (!q) return options;
  return options.filter((option) => {
    const hay = `${optionText(option)} ${typeof option.description === "string" ? option.description : ""} ${option.value}`.toLowerCase();
    return q.split(/\s+/).every((part) => hay.includes(part));
  });
}

/**
 * Keyboard-driven option list. Focus stays on whatever owns the keyboard
 * (a search input or the list itself) and the active row is tracked with
 * aria-activedescendant, the way native comboboxes behave.
 */
export function OptionList<V extends string>({
  options,
  selected,
  onSelect,
  activeIndex,
  onActiveIndexChange,
  emptyText = "没有匹配项",
  id,
  className,
  multiple,
}: {
  options: ListOption<V>[];
  selected?: V | V[] | null;
  onSelect: (value: V) => void;
  activeIndex: number;
  onActiveIndexChange: (index: number) => void;
  emptyText?: ReactNode;
  id: string;
  className?: string;
  multiple?: boolean;
}) {
  const listRef = useRef<HTMLDivElement | null>(null);
  const isSelected = (value: V) => (Array.isArray(selected) ? selected.includes(value) : selected === value);

  useEffect(() => {
    const el = listRef.current?.querySelector<HTMLElement>(`[data-index="${activeIndex}"]`);
    el?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  if (!options.length) {
    return <div className="px-3 py-6 text-center text-[12.5px] text-cx-fg-4">{emptyText}</div>;
  }

  let lastSection: string | undefined;
  return (
    <div ref={listRef} id={id} role="listbox" aria-multiselectable={multiple || undefined} className={cn("cx-scroll flex min-h-0 flex-col overflow-y-auto p-1", className)}>
      {options.map((option, index) => {
        const header = option.section && option.section !== lastSection ? option.section : null;
        lastSection = option.section;
        const active = index === activeIndex;
        const chosen = isSelected(option.value);
        return (
          <div key={option.value} className="contents">
            {header ? <div className="px-2 pb-1 pt-2 text-[11px] font-medium text-cx-fg-4 first:pt-1">{header}</div> : null}
            <div
              id={`${id}-opt-${index}`}
              role="option"
              aria-selected={chosen}
              aria-disabled={option.disabled || undefined}
              data-index={index}
              data-active={active || undefined}
              onPointerMove={() => { if (!option.disabled && !active) onActiveIndexChange(index); }}
              onPointerDown={(event) => event.preventDefault()}
              onClick={() => { if (!option.disabled) onSelect(option.value); }}
              className={cn(
                "flex min-h-8 cursor-default select-none items-center gap-2.5 rounded-lg px-2 py-1.5 text-[13px] leading-5 text-cx-fg",
                "data-[active=true]:bg-cx-hover",
                option.disabled && "opacity-40",
              )}
            >
              {option.leading ?? (option.icon ? <Icon name={option.icon} size={15} className="text-cx-fg-3" /> : null)}
              <span className="flex min-w-0 flex-1 flex-col">
                <span className="truncate">{option.label}</span>
                {option.description ? <span className="truncate text-[11.5px] leading-4 text-cx-fg-3">{option.description}</span> : null}
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

export function useListKeyboard<V extends string>(options: ListOption<V>[], onSelect: (value: V) => void, initial = 0) {
  const [activeIndex, setActiveIndex] = useState(initial);
  useEffect(() => {
    if (activeIndex >= options.length) setActiveIndex(options.length ? 0 : -1);
  }, [activeIndex, options.length]);
  const onKeyDown = (event: React.KeyboardEvent) => {
    if (event.key === "ArrowDown") { event.preventDefault(); setActiveIndex((i) => nextEnabled(options, i, 1)); }
    else if (event.key === "ArrowUp") { event.preventDefault(); setActiveIndex((i) => nextEnabled(options, i < 0 ? 0 : i, -1)); }
    else if (event.key === "Home") { event.preventDefault(); setActiveIndex(nextEnabled(options, -1, 1)); }
    else if (event.key === "End") { event.preventDefault(); setActiveIndex(nextEnabled(options, 0, -1)); }
    else if (event.key === "Enter") {
      const option = options[activeIndex];
      if (option && !option.disabled) { event.preventDefault(); onSelect(option.value); }
    }
  };
  return { activeIndex, setActiveIndex, onKeyDown };
}

export interface SelectProps<V extends string> {
  value: V | null;
  onChange: (value: V) => void;
  options: ListOption<V>[];
  placeholder?: ReactNode;
  /** Custom trigger; defaults to a compact field-style button. */
  trigger?: ReactElement;
  placement?: Placement;
  className?: string;
  popoverClassName?: string;
  size?: "sm" | "md";
  disabled?: boolean;
  ariaLabel?: string;
  searchable?: boolean;
  searchPlaceholder?: string;
  header?: ReactNode;
  footer?: ReactNode | ((close: () => void) => ReactNode);
  emptyText?: ReactNode;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
}

export function Select<V extends string>({
  value,
  onChange,
  options,
  placeholder = "请选择",
  trigger,
  placement = "bottom-start",
  className,
  popoverClassName,
  size = "md",
  disabled,
  ariaLabel,
  searchable,
  searchPlaceholder = "搜索…",
  header,
  footer,
  emptyText,
  open: openProp,
  onOpenChange,
}: SelectProps<V>) {
  const [innerOpen, setInnerOpen] = useState(false);
  const open = openProp ?? innerOpen;
  const setOpen = (next: boolean) => { if (openProp === undefined) setInnerOpen(next); onOpenChange?.(next); };
  const [query, setQuery] = useState("");
  const listId = useId();
  const filtered = useMemo(() => (searchable ? filterOptions(options, query) : options), [options, query, searchable]);
  const current = options.find((option) => option.value === value) ?? null;
  const choose = (next: V) => { onChange(next); setOpen(false); };
  const keyboard = useListKeyboard(filtered, choose);

  useEffect(() => {
    if (!open) { setQuery(""); return; }
    const index = filtered.findIndex((option) => option.value === value);
    keyboard.setActiveIndex(index >= 0 ? index : 0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const defaultTrigger = (
    <button
      type="button"
      disabled={disabled}
      aria-label={ariaLabel}
      className={cn(
        "cx-press inline-flex min-w-0 items-center gap-2 rounded-lg border border-cx-border bg-cx-elevated text-left text-cx-fg shadow-cx-xs hover:border-cx-border-strong disabled:opacity-50",
        size === "sm" ? "h-7 px-2 text-[12.5px]" : "h-9 px-3 text-[13px]",
        className,
      )}
    >
      {current?.icon ? <Icon name={current.icon} size={14} className="text-cx-fg-3" /> : current?.leading}
      <span className={cn("min-w-0 flex-1 truncate", !current && "text-cx-fg-4")}>{current?.label ?? placeholder}</span>
      <Icon name="chevronsUpDown" size={13} className="text-cx-fg-4" />
    </button>
  );

  return (
    <Popover
      open={open}
      onOpenChange={setOpen}
      trigger={trigger ?? defaultTrigger}
      placement={placement}
      matchWidth={!trigger}
      role="dialog"
      haspopup="listbox"
      initialFocus={searchable ? "first" : "container"}
      className={cn("max-w-[min(420px,calc(100vw-16px))]", popoverClassName)}
      onKeyDown={searchable ? undefined : keyboard.onKeyDown}
    >
      {({ close }) => (
        <>
          {header}
          {searchable ? (
            <div className="border-b border-cx-border-subtle p-1.5">
              <input
                data-autofocus
                value={query}
                onChange={(event) => { setQuery(event.target.value); keyboard.setActiveIndex(0); }}
                onKeyDown={keyboard.onKeyDown}
                placeholder={searchPlaceholder}
                role="combobox"
                aria-expanded
                aria-controls={listId}
                aria-activedescendant={keyboard.activeIndex >= 0 ? `${listId}-opt-${keyboard.activeIndex}` : undefined}
                className="h-8 w-full rounded-md bg-transparent px-2 text-[13px] text-cx-fg outline-none placeholder:text-cx-fg-4"
              />
            </div>
          ) : null}
          <OptionList
            id={listId}
            options={filtered}
            selected={value}
            onSelect={choose}
            activeIndex={keyboard.activeIndex}
            onActiveIndexChange={keyboard.setActiveIndex}
            emptyText={emptyText}
          />
          {footer ? (
            <div className="border-t border-cx-border-subtle p-1">{typeof footer === "function" ? footer(close) : footer}</div>
          ) : null}
        </>
      )}
    </Popover>
  );
}
