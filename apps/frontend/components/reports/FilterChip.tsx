"use client";

import type { ReactNode } from "react";
import { Icon } from "@muteki/ui";

export function FilterChip({ label, removeLabel, onRemove }: { label: ReactNode; removeLabel: string; onRemove: () => void }) {
  return (
    <span className="inline-flex h-6 max-w-full items-center gap-1 rounded-md bg-cx-accent-soft pl-2 pr-1 text-cx-xs text-cx-accent">
      <span className="truncate">{label}</span>
      <button
        type="button"
        aria-label={removeLabel}
        onClick={onRemove}
        className="grid size-4 shrink-0 place-items-center rounded hover:bg-[color-mix(in_srgb,var(--cx-accent)_16%,transparent)]"
      >
        <Icon name="x" size={11} />
      </button>
    </span>
  );
}
