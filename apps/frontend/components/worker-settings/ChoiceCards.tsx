// ai-design-check-ignore-file use-shared-components -- a radio card, not a command button
"use client";

import type { ReactNode } from "react";
import { Icon, type IconName } from "@muteki/ui";
import { cn } from "@/lib/cn";

export interface Choice<V extends string> {
  value: V;
  title: ReactNode;
  description?: ReactNode;
  icon?: IconName;
  disabled?: boolean;
}

export function ChoiceCards<V extends string>({
  value, onChange, options, ariaLabel,
}: {
  value: V;
  onChange: (value: V) => void;
  options: Choice<V>[];
  ariaLabel: string;
}) {
  return (
    <div role="radiogroup" aria-label={ariaLabel} className="grid gap-2 @[560px]:grid-cols-2">
      {options.map((option) => {
        const selected = option.value === value;
        return (
          <button
            key={option.value}
            type="button"
            role="radio"
            aria-checked={selected}
            disabled={option.disabled}
            onClick={() => onChange(option.value)}
            className={cn(
              "cx-press flex items-start gap-3 rounded-xl border px-3.5 py-3 text-left outline-none transition-colors",
              "focus-visible:ring-2 focus-visible:ring-[var(--cx-focus)] disabled:cursor-not-allowed disabled:opacity-50",
              selected
                ? "border-cx-accent-line bg-cx-accent-soft"
                : "border-cx-border bg-cx-elevated hover:border-cx-border-strong hover:bg-cx-hover",
            )}
          >
            {option.icon ? (
              <span className={cn("mt-px grid size-8 shrink-0 place-items-center rounded-lg border bg-cx-bg", selected ? "border-cx-accent-line text-cx-accent" : "border-cx-border text-cx-fg-3")}>
                <Icon name={option.icon} size={16} />
              </span>
            ) : null}
            <span className="min-w-0 flex-1">
              <span className="block text-cx-sm font-medium leading-5 text-cx-fg">{option.title}</span>
              {option.description ? <span className="mt-0.5 block text-cx-xs leading-[18px] text-cx-fg-3">{option.description}</span> : null}
            </span>
            <span
              aria-hidden
              className={cn(
                "mt-1 grid size-4 shrink-0 place-items-center rounded-full border",
                selected ? "border-cx-accent bg-cx-accent text-cx-accent-fg" : "border-cx-border-strong bg-cx-elevated",
              )}
            >
              {selected ? <Icon name="check" size={10} /> : null}
            </span>
          </button>
        );
      })}
    </div>
  );
}
