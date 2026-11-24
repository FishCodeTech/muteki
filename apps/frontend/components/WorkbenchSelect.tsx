"use client";

import { Select, type ListOption } from "@muteki/ui";
import { cn } from "@/lib/cn";

type WorkbenchSelectOption = {
  value: string;
  label: string;
  description?: string;
};

export function WorkbenchSelect({
  ariaLabel,
  value,
  onChange,
  options,
  placeholder,
  disabled,
  className,
}: {
  ariaLabel: string;
  value: string;
  onChange: (value: string) => void;
  options: WorkbenchSelectOption[];
  placeholder?: string;
  disabled?: boolean;
  className?: string;
}) {
  const listOptions: ListOption<string>[] = options.map((option) => ({
    value: option.value,
    label: option.label,
    description: option.description,
  }));
  return (
    <Select
      ariaLabel={ariaLabel}
      size="sm"
      className={cn("workbench-select-trigger min-w-0 max-w-full", className)}
      value={options.some((option) => option.value === value) ? value : undefined}
      onChange={onChange}
      options={listOptions}
      placeholder={placeholder}
      disabled={disabled}
      popoverClassName="max-w-[min(26rem,calc(100vw-1rem))]"
    />
  );
}
