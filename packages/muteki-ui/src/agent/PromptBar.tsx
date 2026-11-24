"use client";

import type { KeyboardEvent, ReactNode } from "react";
import { cn } from "../utils/cn";
import { Icon } from "../icons/Icon";
import { Button, IconButton } from "../primitives/Button";
import { Select, type ListOption } from "../primitives/ListBox";
import { SegmentedControl, type SegmentOption } from "../primitives/Tabs";

export interface AgentPromptBarProps {
  value: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  onStop?: () => void;
  streaming?: boolean;
  disabled?: boolean;
  submitDisabled?: boolean;
  placeholder?: string;
  leadingTools?: ReactNode;
  trailingTools?: ReactNode;
  topSlot?: ReactNode;
  bottomSlot?: ReactNode;
  submitLabel?: string;
  stopLabel?: string;
  className?: string;
}

export function AgentPromptBar({
  value,
  onChange,
  onSubmit,
  onStop,
  streaming = false,
  disabled = false,
  submitDisabled,
  placeholder = "输入指令或追问…",
  leadingTools,
  trailingTools,
  topSlot,
  bottomSlot,
  submitLabel = "发送",
  stopLabel = "停止",
  className,
}: AgentPromptBarProps) {
  const isSubmitDisabled = submitDisabled ?? (disabled || !value.trim());

  const handleKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.nativeEvent.isComposing || event.keyCode === 229) return;
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      if (!isSubmitDisabled) onSubmit();
    }
  };

  return (
    <div
      data-slot="agent-prompt-bar"
      className={cn(
        "ui-surface-ring flex w-full flex-col overflow-hidden rounded-[14px] bg-cx-elevated transition-[box-shadow] duration-150",
        "focus-within:[--ui-surface-ring:var(--cx-ring-strong)] focus-within:shadow-[0_0_0_2px_var(--cx-focus)]",
        className,
      )}
    >
      {topSlot ? <div className="border-b border-cx-border-subtle px-3 py-2">{topSlot}</div> : null}
      <textarea
        data-slot="prompt-textarea"
        value={value}
        disabled={disabled}
        rows={3}
        placeholder={placeholder}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={handleKeyDown}
        className="cx-scroll w-full resize-none border-0 bg-transparent px-3.5 pt-3 pb-2 text-cx-sm leading-6 text-cx-fg placeholder:text-cx-fg-4 outline-none disabled:opacity-50"
      />
      <div className="flex flex-wrap items-center justify-between gap-2 px-2.5 pb-2.5 pt-1">
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">{leadingTools}</div>
        <div className="flex items-center gap-1.5">
          {trailingTools}
          {streaming && onStop ? (
            <Button
              size="sm"
              variant="danger-soft"
              icon="stop"
              agentConfirm="destructive"
              onClick={onStop}
              aria-label={stopLabel}
            >
              {stopLabel}
            </Button>
          ) : (
            <IconButton
              icon="sendUp"
              size="sm"
              variant="primary"
              label={submitLabel}
              disabled={isSubmitDisabled}
              onClick={onSubmit}
            />
          )}
        </div>
      </div>
      {bottomSlot ? (
        <div className="border-t border-cx-border-subtle bg-cx-bg-subtle/60 px-3 py-1.5 text-cx-xs text-cx-fg-3">
          {bottomSlot}
        </div>
      ) : null}
    </div>
  );
}

export interface AgentModelOption {
  value: string;
  label: string;
  description?: string;
  disabled?: boolean;
}

export function ModelPicker({
  value,
  onChange,
  options,
  ariaLabel = "选择模型",
  disabled,
  className,
}: {
  value: string;
  onChange: (value: string) => void;
  options: AgentModelOption[];
  ariaLabel?: string;
  disabled?: boolean;
  className?: string;
}) {
  const mapped: ListOption<string>[] = options.map((opt) => ({
    value: opt.value,
    label: opt.label,
    description: opt.description,
    disabled: opt.disabled,
    icon: "cpu",
  }));
  return (
    <div data-slot="model-picker" className={className}>
      <Select
        value={value}
        onChange={onChange}
        options={mapped}
        ariaLabel={ariaLabel}
        size="xs"
        disabled={disabled}
      />
    </div>
  );
}

export type ReasoningEffort = "low" | "medium" | "high" | "max";

const DEFAULT_EFFORT_OPTIONS: SegmentOption<ReasoningEffort>[] = [
  { value: "low", label: "低" },
  { value: "medium", label: "中" },
  { value: "high", label: "高" },
  { value: "max", label: "极高" },
];

export function EffortSlider({
  value,
  onChange,
  options = DEFAULT_EFFORT_OPTIONS,
  ariaLabel = "推理强度",
  className,
}: {
  value: ReasoningEffort;
  onChange: (value: ReasoningEffort) => void;
  options?: SegmentOption<ReasoningEffort>[];
  ariaLabel?: string;
  className?: string;
}) {
  return (
    <div data-slot="effort-slider" className={cn("inline-flex items-center gap-1.5", className)}>
      <Icon name="gauge" size={13} className="text-cx-fg-4" />
      <SegmentedControl<ReasoningEffort>
        value={value}
        onChange={onChange}
        options={options}
        size="xs"
        ariaLabel={ariaLabel}
      />
    </div>
  );
}

export function ContextMeter({
  used,
  limit,
  label,
  className,
}: {
  used: number;
  limit: number;
  label?: ReactNode;
  className?: string;
}) {
  const safeLimit = Math.max(1, limit);
  const ratio = Math.min(1, Math.max(0, used / safeLimit));
  const percent = Math.round(ratio * 100);
  const tone = ratio >= 0.9 ? "text-cx-danger" : ratio >= 0.72 ? "text-cx-warning" : "text-cx-fg-3";

  return (
    <div
      data-slot="context-meter"
      role="meter"
      aria-valuenow={used}
      aria-valuemin={0}
      aria-valuemax={safeLimit}
      aria-label={typeof label === "string" ? label : "上下文占用"}
      className={cn("inline-flex items-center gap-2 text-cx-xs", className)}
    >
      <div className="h-1.5 w-16 overflow-hidden rounded-full bg-cx-hover">
        <div
          className={cn(
            "h-full rounded-full transition-[width] duration-200 ease-cx-out",
            ratio >= 0.9 ? "bg-cx-danger" : ratio >= 0.72 ? "bg-cx-warning" : "bg-cx-accent",
          )}
          style={{ width: `${percent}%` }}
        />
      </div>
      <span className={cn("cx-tabular font-cx-mono", tone)}>
        {label ?? `${percent}%`}
      </span>
    </div>
  );
}
