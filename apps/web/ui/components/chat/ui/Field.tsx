"use client";

import {
  useId,
  useLayoutEffect,
  useRef,
  type ComponentPropsWithRef,
  type ReactNode,
  type Ref,
} from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Checkbox as AgentCheckbox } from "@/components/agentui/motion/checkbox";

const CONTROL =
  "w-full rounded-lg border border-cx-border bg-cx-elevated text-[13.5px] text-cx-fg placeholder:text-cx-fg-4 outline-none transition-[border-color,box-shadow] duration-150 " +
  "hover:border-cx-border-strong focus:border-cx-border-strong disabled:opacity-50";

export function Label({ children, htmlFor, className, hint }: { children: ReactNode; htmlFor?: string; className?: string; hint?: ReactNode }) {
  return (
    <label htmlFor={htmlFor} className={cn("mb-1.5 flex items-center justify-between text-[12.5px] font-medium text-cx-fg-2", className)}>
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
}

export function Input({ icon, trailing, size = "md", invalid, className, ...rest }: InputProps) {
  const height = size === "sm" ? "h-8" : size === "lg" ? "h-11 text-[14px]" : "h-9";
  if (!icon && !trailing) {
    return <input {...rest} aria-invalid={invalid || undefined} className={cn(CONTROL, height, "px-3", invalid && "border-cx-danger", className)} />;
  }
  return (
    <div className={cn("relative flex items-center", className)}>
      {icon ? <Icon name={icon} size={15} className="pointer-events-none absolute left-2.5 text-cx-fg-4" /> : null}
      <input
        {...rest}
        aria-invalid={invalid || undefined}
        className={cn(CONTROL, height, icon ? "pl-8" : "pl-3", trailing ? "pr-9" : "pr-3", invalid && "border-cx-danger")}
      />
      {trailing ? <div className="absolute right-1.5 flex items-center">{trailing}</div> : null}
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
    <div className={cn("flex flex-col", containerClassName)}>
      {label ? <Label htmlFor={inputId} hint={labelHint}>{label}</Label> : null}
      <Input id={inputId} invalid={Boolean(error)} {...rest} />
      {error ? (
        <p className="mt-1.5 text-[12px] text-cx-danger">{error}</p>
      ) : description ? (
        <p className="mt-1.5 text-[12px] leading-4 text-cx-fg-4">{description}</p>
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
}

function assignRef<T>(ref: Ref<T> | undefined, value: T) {
  if (!ref) return;
  if (typeof ref === "function") ref(value);
  else (ref as { current: T }).current = value;
}

export function TextArea({ label, description, autoResize, maxRows = 12, containerClassName, className, id, ref, ...rest }: TextAreaProps) {
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
    <div className={cn("flex flex-col", containerClassName)}>
      {label ? <Label htmlFor={areaId}>{label}</Label> : null}
      <textarea
        {...rest}
        id={areaId}
        ref={(node) => { innerRef.current = node; assignRef(ref, node); }}
        className={cn(CONTROL, "cx-scroll min-h-[72px] resize-y px-3 py-2 leading-5", autoResize && "resize-none", className)}
      />
      {description ? <p className="mt-1.5 text-[12px] text-cx-fg-4">{description}</p> : null}
    </div>
  );
}

export interface SearchInputProps extends Omit<InputProps, "icon" | "trailing" | "onChange"> {
  value: string;
  onValueChange: (value: string) => void;
  shortcutHint?: ReactNode;
}

export function SearchInput({ value, onValueChange, shortcutHint, size = "sm", className, ...rest }: SearchInputProps) {
  return (
    <Input
      {...rest}
      size={size}
      icon="search"
      value={value}
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
          aria-label="清除搜索"
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
}: {
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  label?: ReactNode;
  description?: ReactNode;
  disabled?: boolean;
  indeterminate?: boolean;
  className?: string;
  id?: string;
}) {
  const autoId = useId();
  const boxId = id ?? autoId;
  return (
    <div className={cn("flex items-start gap-2.5", disabled && "opacity-50", className)}>
      <AgentCheckbox
        id={boxId}
        checked={checked}
        indeterminate={indeterminate}
        disabled={disabled}
        onCheckedChange={onCheckedChange}
        aria-label={typeof label === "string" ? label : undefined}
        className="mt-px [&>button]:size-[18px] [&>button]:rounded-[6px] [&>button]:border-[1.5px]"
      />
      {label || description ? (
        <label htmlFor={boxId} className={cn("flex min-w-0 cursor-pointer flex-col", disabled && "cursor-not-allowed")}>
          {label ? <span className="text-[13px] leading-5 text-cx-fg">{label}</span> : null}
          {description ? <span className="text-[12px] leading-4 text-cx-fg-3">{description}</span> : null}
        </label>
      ) : null}
    </div>
  );
}

export function Switch({
  checked,
  onCheckedChange,
  label,
  description,
  disabled,
  size = "md",
  className,
}: {
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  label?: ReactNode;
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
      aria-checked={checked}
      disabled={disabled}
      onClick={() => onCheckedChange(!checked)}
      className={cn(
        "relative inline-flex shrink-0 items-center rounded-full p-0.5 transition-colors duration-200 disabled:opacity-50",
        track,
        checked ? "bg-cx-fg-2" : "bg-cx-border-strong",
      )}
    >
      <span
        data-on={checked}
        className={cn("rounded-full shadow-[0_1px_2px_rgb(0_0_0/0.25)] transition-transform duration-200 ease-cx-out", checked ? "bg-cx-bg" : "bg-cx-fg-3", knob)}
      />
    </button>
  );
  if (!label) return control;
  return (
    <div className={cn("flex items-center justify-between gap-4", className)}>
      <span className="flex min-w-0 flex-col">
        <span id={labelId} className="text-[13px] text-cx-fg">{label}</span>
        {description ? <span className="text-[12px] leading-4 text-cx-fg-3">{description}</span> : null}
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
    <div className={cn("flex flex-col gap-2", className)}>
      {label || valueLabel ? (
        <div className="flex items-center justify-between text-[12.5px]">
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
        <div className="relative h-4 text-[11px] text-cx-fg-4">
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
