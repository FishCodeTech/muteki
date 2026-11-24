"use client";

import {
  useId,
  useLayoutEffect,
  useRef,
  type ComponentPropsWithRef,
  type KeyboardEvent,
  type ReactNode,
  type Ref,
} from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "../utils/cn";
import { useReducedMotion } from "../utils/reducedMotion";
import { Icon, type IconName } from "../icons/Icon";
import { EASE_OUT, SPRING_LAYOUT } from "./motion";

const CONTROL =
  "cx-field ui-surface-tactile w-full rounded-[8px] border border-cx-border bg-cx-elevated text-cx-sm text-cx-fg placeholder:text-cx-fg-4 outline-none transition-[border-color,box-shadow] duration-150 " +
  "hover:border-cx-border-strong focus:border-cx-border-strong focus:shadow-[0_0_0_2px_var(--cx-focus)] disabled:cursor-not-allowed disabled:opacity-50";

const HEIGHT = { sm: "h-7", md: "h-8", lg: "h-10 text-cx-base" } as const;

export function Label({ children, htmlFor, className, hint }: { children: ReactNode; htmlFor?: string; className?: string; hint?: ReactNode }) {
  return (
    <label htmlFor={htmlFor} data-slot="label" className={cn("mb-1.5 flex items-center justify-between gap-3 text-cx-xs font-medium text-cx-fg-2", className)}>
      <span>{children}</span>
      {hint ? <span className="font-normal text-cx-fg-4">{hint}</span> : null}
    </label>
  );
}

export interface InputProps extends Omit<ComponentPropsWithRef<"input">, "size"> {
  icon?: IconName;
  trailing?: ReactNode;
  size?: "sm" | "md" | "lg";
  invalid?: boolean;
  /** Semantic marker for secrets/API keys/tokens (`data-agent-secret="true"`). */
  agentSecret?: boolean;
}

export function Input({ icon, trailing, size = "md", invalid, agentSecret, className, ...rest }: InputProps) {
  const height = HEIGHT[size];
  const isSecret = Boolean(agentSecret);
  if (!icon && !trailing) {
    return (
      <input
        {...rest}
        aria-invalid={invalid || undefined}
        data-slot="input"
        data-invalid={invalid ? "true" : undefined}
        data-disabled={rest.disabled ? "true" : undefined}
        data-agent-secret={isSecret ? "true" : undefined}
        className={cn(CONTROL, height, "px-3", invalid && "border-cx-danger focus:border-cx-danger", className)}
      />
    );
  }
  return (
    <div className={cn("relative flex items-center", className)}>
      {icon ? <Icon name={icon} size={15} className="pointer-events-none absolute left-2.5 text-cx-fg-4" /> : null}
      <input
        {...rest}
        aria-invalid={invalid || undefined}
        data-slot="input"
        data-invalid={invalid ? "true" : undefined}
        data-disabled={rest.disabled ? "true" : undefined}
        data-agent-secret={isSecret ? "true" : undefined}
        className={cn(CONTROL, height, icon ? "pl-8" : "pl-3", trailing ? "pr-9" : "pr-3", invalid && "border-cx-danger focus:border-cx-danger")}
      />
      {trailing ? <div className="absolute right-1.5 flex items-center">{trailing}</div> : null}
    </div>
  );
}

export interface NativeSelectProps extends Omit<ComponentPropsWithRef<"select">, "size"> {
  size?: "sm" | "md" | "lg";
  invalid?: boolean;
}

export function NativeSelect({ size = "md", invalid, className, children, ...rest }: NativeSelectProps) {
  return (
    <div className={cn("relative flex items-center", className)}>
      <select
        {...rest}
        aria-invalid={invalid || undefined}
        data-slot="native-select"
        data-invalid={invalid ? "true" : undefined}
        className={cn(CONTROL, HEIGHT[size], "cursor-pointer appearance-none truncate pl-3 pr-8", invalid && "border-cx-danger")}
      >
        {children}
      </select>
      <Icon name="chevronDown" size={14} className="pointer-events-none absolute right-2.5 text-cx-fg-4" />
    </div>
  );
}

export interface TextFieldProps extends InputProps {
  label?: ReactNode;
  description?: ReactNode;
  error?: ReactNode;
  labelHint?: ReactNode;
  containerClassName?: string;
}

export function TextField({ label, description, error, labelHint, containerClassName, id, ...rest }: TextFieldProps) {
  const autoId = useId();
  const inputId = id ?? autoId;
  return (
    <div data-slot="text-field" className={cn("flex flex-col", containerClassName)}>
      {label ? <Label htmlFor={inputId} hint={labelHint}>{label}</Label> : null}
      <Input id={inputId} invalid={Boolean(error) || rest.invalid} {...rest} />
      {error ? (
        <p role="alert" className="mt-1.5 text-cx-xs text-cx-danger">{error}</p>
      ) : description ? (
        <p className="mt-1.5 text-cx-xs leading-4 text-cx-fg-4">{description}</p>
      ) : null}
    </div>
  );
}

export interface TextAreaProps extends ComponentPropsWithRef<"textarea"> {
  label?: ReactNode;
  description?: ReactNode;
  autoResize?: boolean;
  maxRows?: number;
  containerClassName?: string;
  agentSecret?: boolean;
}

function assignRef<T>(ref: Ref<T> | undefined, value: T) {
  if (!ref) return;
  if (typeof ref === "function") ref(value);
  else (ref as { current: T }).current = value;
}

export function TextArea({ label, description, autoResize, maxRows = 12, containerClassName, agentSecret, className, id, ref, ...rest }: TextAreaProps) {
  const autoId = useId();
  const areaId = id ?? autoId;
  const innerRef = useRef<HTMLTextAreaElement | null>(null);
  useLayoutEffect(() => {
    if (!autoResize) return;
    const el = innerRef.current;
    if (!el) return;
    el.style.height = "auto";
    const line = parseFloat(getComputedStyle(el).lineHeight) || 20;
    el.style.height = `${Math.min(el.scrollHeight, line * maxRows + 16)}px`;
  }, [autoResize, maxRows, rest.value]);
  return (
    <div data-slot="textarea-field" className={cn("flex flex-col", containerClassName)}>
      {label ? <Label htmlFor={areaId}>{label}</Label> : null}
      <textarea
        {...rest}
        id={areaId}
        data-slot="textarea"
        data-agent-secret={agentSecret ? "true" : undefined}
        ref={(node) => { innerRef.current = node; assignRef(ref, node); }}
        className={cn(CONTROL, "cx-scroll min-h-[72px] resize-y px-3 py-2 leading-5", autoResize && "resize-none", className)}
      />
      {description ? <p className="mt-1.5 text-cx-xs text-cx-fg-4">{description}</p> : null}
    </div>
  );
}

export interface SearchInputProps extends Omit<InputProps, "icon" | "trailing" | "onChange"> {
  value: string;
  onValueChange: (value: string) => void;
  shortcutHint?: ReactNode;
  clearLabel?: string;
}

export function SearchInput({ value, onValueChange, shortcutHint, clearLabel = "清除搜索", size = "sm", className, ...rest }: SearchInputProps) {
  return (
    <Input
      {...rest}
      size={size}
      icon="search"
      value={value}
      data-escape-clears={value ? "" : undefined}
      onChange={(event) => onValueChange(event.target.value)}
      onKeyDown={(event) => {
        if (event.key === "Escape" && value) {
          event.stopPropagation();
          event.preventDefault();
          onValueChange("");
        }
        rest.onKeyDown?.(event);
      }}
      className={className}
      trailing={value ? (
        <button
          type="button"
          aria-label={clearLabel}
          onClick={() => onValueChange("")}
          className="grid size-6 place-items-center rounded-md text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"
        >
          <Icon name="x" size={13} />
        </button>
      ) : shortcutHint ? <span className="pr-1">{shortcutHint}</span> : null}
    />
  );
}

export function Checkbox({
  checked,
  onCheckedChange,
  label,
  description,
  disabled,
  indeterminate,
  className,
  id,
  ariaLabel,
  testId,
}: {
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  label?: ReactNode;
  description?: ReactNode;
  disabled?: boolean;
  indeterminate?: boolean;
  className?: string;
  id?: string;
  ariaLabel?: string;
  testId?: string;
}) {
  const autoId = useId();
  const boxId = id ?? autoId;
  const active = checked || indeterminate;
  const reduced = useReducedMotion();
  return (
    <div data-slot="checkbox-field" data-selected={checked ? "true" : undefined} data-disabled={disabled ? "true" : undefined} className={cn("flex items-start gap-2.5", disabled && "opacity-50", className)}>
      <button
        id={boxId}
        type="button"
        role="checkbox"
        aria-checked={indeterminate ? "mixed" : checked}
        aria-label={ariaLabel ?? (typeof label === "string" ? label : undefined)}
        disabled={disabled}
        data-slot="checkbox"
        data-selected={checked ? "true" : undefined}
        data-disabled={disabled ? "true" : undefined}
        data-testid={testId}
        onClick={() => !disabled && onCheckedChange(!checked)}
        className={cn(
          "cx-press mt-[2px] inline-flex size-[17px] shrink-0 items-center justify-center rounded-[5px] border outline-none transition-[background-color,border-color,box-shadow] duration-150",
          "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
          "disabled:cursor-not-allowed",
          active
            ? "border-cx-accent bg-cx-accent text-cx-accent-fg shadow-[inset_0_1px_0_color-mix(in_srgb,white_20%,transparent)]"
            : "border-cx-border-strong bg-cx-elevated hover:border-cx-fg-3",
        )}
      >
        <AnimatePresence initial={false}>
          {indeterminate ? (
            <motion.svg
              key="indeterminate"
              width={11}
              height={11}
              viewBox="0 0 12 12"
              fill="none"
              stroke="currentColor"
              strokeWidth={2}
              strokeLinecap="round"
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
              transition={{ duration: reduced ? 0.08 : 0.14, ease: EASE_OUT }}
            >
              <motion.path
                d="M2.5 6h7"
                initial={reduced ? { pathLength: 1 } : { pathLength: 0 }}
                animate={{ pathLength: 1 }}
                transition={{ duration: reduced ? 0.08 : 0.18, ease: EASE_OUT }}
              />
            </motion.svg>
          ) : checked ? (
            <motion.svg
              key="check"
              width={12}
              height={12}
              viewBox="0 0 12 12"
              fill="none"
              stroke="currentColor"
              strokeWidth={1.85}
              strokeLinecap="round"
              strokeLinejoin="round"
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
              transition={{ duration: reduced ? 0.08 : 0.14, ease: EASE_OUT }}
            >
              <motion.path
                d="M2.25 6.25l2.5 2.5 5-5.25"
                initial={reduced ? { pathLength: 1 } : { pathLength: 0 }}
                animate={{ pathLength: 1 }}
                transition={{ duration: reduced ? 0.08 : 0.24, ease: EASE_OUT }}
              />
            </motion.svg>
          ) : null}
        </AnimatePresence>
      </button>
      {label || description ? (
        <label htmlFor={boxId} className={cn("flex min-w-0 cursor-pointer flex-col select-none", disabled && "cursor-not-allowed")}>
          {label ? <span className="text-cx-sm leading-5 text-cx-fg">{label}</span> : null}
          {description ? <span className="text-cx-xs leading-4 text-cx-fg-3">{description}</span> : null}
        </label>
      ) : null}
    </div>
  );
}

export interface RadioOption<V extends string = string> {
  value: V;
  label: ReactNode;
  description?: ReactNode;
  disabled?: boolean;
}

export function RadioGroup<V extends string = string>({
  value,
  onValueChange,
  options,
  ariaLabel,
  disabled = false,
  className,
}: {
  value: V;
  onValueChange: (value: V) => void;
  options: RadioOption<V>[];
  ariaLabel?: string;
  disabled?: boolean;
  className?: string;
}) {
  const groupId = useId();
  const reduced = useReducedMotion();
  const buttonRefs = useRef<(HTMLButtonElement | null)[]>([]);
  const enabledIndices = options
    .map((opt, idx) => (!disabled && !opt.disabled ? idx : -1))
    .filter((idx) => idx >= 0);
  const selectedEnabled = options.findIndex(
    (opt) => opt.value === value && !disabled && !opt.disabled,
  );
  const tabbableIndex = selectedEnabled >= 0 ? selectedEnabled : (enabledIndices[0] ?? -1);

  const handleKeyDown = (event: KeyboardEvent<HTMLButtonElement>, currentIndex: number) => {
    if (enabledIndices.length === 0) return;
    let targetIndex: number | undefined;
    if (event.key === "ArrowDown" || event.key === "ArrowRight") {
      const pos = enabledIndices.indexOf(currentIndex);
      targetIndex = enabledIndices[(pos + 1) % enabledIndices.length];
    } else if (event.key === "ArrowUp" || event.key === "ArrowLeft") {
      const pos = enabledIndices.indexOf(currentIndex);
      targetIndex = enabledIndices[(pos - 1 + enabledIndices.length) % enabledIndices.length];
    } else if (event.key === "Home") {
      targetIndex = enabledIndices[0];
    } else if (event.key === "End") {
      targetIndex = enabledIndices[enabledIndices.length - 1];
    }
    if (targetIndex == null) return;
    event.preventDefault();
    const targetOption = options[targetIndex];
    if (!targetOption) return;
    buttonRefs.current[targetIndex]?.focus();
    onValueChange(targetOption.value);
  };

  return (
    <div
      role="radiogroup"
      aria-label={ariaLabel}
      aria-disabled={disabled ? "true" : undefined}
      data-slot="radio-group"
      data-disabled={disabled ? "true" : undefined}
      className={cn("flex flex-col gap-2", className)}
    >
      {options.map((opt, idx) => {
        const selected = opt.value === value;
        const itemDisabled = Boolean(disabled || opt.disabled);
        return (
          <button
            key={opt.value}
            ref={(el) => {
              buttonRefs.current[idx] = el;
            }}
            type="button"
            role="radio"
            aria-checked={selected}
            aria-disabled={itemDisabled ? "true" : undefined}
            disabled={itemDisabled}
            data-selected={selected ? "true" : undefined}
            data-disabled={itemDisabled ? "true" : undefined}
            tabIndex={itemDisabled ? undefined : idx === tabbableIndex ? 0 : -1}
            onClick={() => !itemDisabled && onValueChange(opt.value)}
            onKeyDown={(event) => handleKeyDown(event, idx)}
            className={cn(
              "cx-press flex items-start gap-2.5 rounded-[10px] border p-2.5 text-left outline-none transition-colors",
              "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)] disabled:cursor-not-allowed disabled:opacity-50",
              selected
                ? "border-cx-accent-line bg-cx-accent-soft"
                : "border-cx-border bg-cx-elevated hover:border-cx-border-strong",
            )}
          >
            <span
              className={cn(
                "mt-[3px] grid size-4 shrink-0 place-items-center rounded-full border transition-colors duration-150 ease-cx-out",
                selected ? "border-cx-accent bg-cx-accent" : "border-cx-border-strong bg-cx-bg",
              )}
            >
              {selected ? (
                <motion.span
                  layoutId={`radio-dot-${groupId}`}
                  transition={reduced ? { duration: 0 } : SPRING_LAYOUT}
                  className="size-1.5 rounded-full bg-cx-accent-fg"
                />
              ) : null}
            </span>
            <span className="flex min-w-0 flex-1 flex-col">
              <span className="text-cx-sm font-medium text-cx-fg">{opt.label}</span>
              {opt.description ? <span className="text-cx-xs leading-4 text-cx-fg-3">{opt.description}</span> : null}
            </span>
          </button>
        );
      })}
    </div>
  );
}

export function Switch({
  checked,
  onCheckedChange,
  label,
  ariaLabel,
  description,
  disabled,
  size = "md",
  className,
}: {
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  label?: ReactNode;
  ariaLabel?: string;
  description?: ReactNode;
  disabled?: boolean;
  size?: "sm" | "md";
  className?: string;
}) {
  const track = size === "sm" ? "h-4 w-7" : "h-5 w-9";
  const labelId = useId();
  const knob = size === "sm" ? "size-3 data-[on=true]:translate-x-3" : "size-4 data-[on=true]:translate-x-4";
  const control = (
    <button
      type="button"
      role="switch"
      aria-labelledby={label ? labelId : undefined}
      aria-label={label ? undefined : ariaLabel}
      aria-checked={checked}
      disabled={disabled}
      data-slot="switch"
      data-selected={checked ? "true" : undefined}
      data-disabled={disabled ? "true" : undefined}
      onClick={() => onCheckedChange(!checked)}
      className={cn(
        "relative inline-flex shrink-0 items-center rounded-full p-0.5 transition-colors duration-200 disabled:opacity-50",
        "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
        track,
        checked ? "bg-cx-accent" : "bg-cx-border-strong",
      )}
    >
      <span
        data-on={checked}
        className={cn("rounded-full bg-white shadow-[0_1px_2px_rgb(0_0_0/0.25)] transition-transform duration-200 ease-cx-out", knob)}
      />
    </button>
  );
  if (!label) return control;
  return (
    <div className={cn("flex items-center justify-between gap-4", className)}>
      <span className="flex min-w-0 flex-col">
        <span id={labelId} className="text-cx-sm text-cx-fg">{label}</span>
        {description ? <span className="text-cx-xs leading-4 text-cx-fg-3">{description}</span> : null}
      </span>
      {control}
    </div>
  );
}

export function Slider({
  value,
  onValueChange,
  min = 0,
  max = 100,
  step = 1,
  label,
  valueLabel,
  marks,
  className,
  disabled,
}: {
  value: number;
  onValueChange: (value: number) => void;
  min?: number;
  max?: number;
  step?: number;
  label?: ReactNode;
  valueLabel?: ReactNode;
  marks?: Array<{ value: number; label: string }>;
  className?: string;
  disabled?: boolean;
}) {
  const pct = ((value - min) / Math.max(1, max - min)) * 100;
  return (
    <div data-slot="slider" className={cn("flex flex-col gap-2", className)}>
      {label || valueLabel ? (
        <div className="flex items-center justify-between text-cx-xs">
          <span className="font-medium text-cx-fg-2">{label}</span>
          <span className="cx-tabular text-cx-fg-3">{valueLabel ?? value}</span>
        </div>
      ) : null}
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        disabled={disabled}
        onChange={(event) => onValueChange(Number(event.target.value))}
        className="cx-slider"
        style={{ "--cx-slider-pct": `${pct}%` } as React.CSSProperties}
      />
      {marks?.length ? (
        <div className="relative h-4 text-cx-2xs text-cx-fg-4">
          {marks.map((mark) => (
            <button
              key={mark.value}
              type="button"
              onClick={() => onValueChange(mark.value)}
              className={cn("absolute -translate-x-1/2 hover:text-cx-fg", mark.value === value && "font-medium text-cx-fg-2")}
              style={{ left: `${((mark.value - min) / Math.max(1, max - min)) * 100}%` }}
            >
              {mark.label}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}
