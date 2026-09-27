import { cn } from "@/lib/cn";

/** Shared look for the subtle pills in the strip under the composer. */
export function stripPillClass(interactive: boolean): string {
  return cn(
    "inline-flex h-7 min-w-0 items-center gap-1.5 rounded-full px-2.5 text-[12.5px] font-medium text-cx-fg-3",
    "outline-none focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-[var(--cx-focus)]",
    interactive
      ? "cx-press hover:bg-cx-hover hover:text-cx-fg data-[state=open]:bg-cx-active data-[state=open]:text-cx-fg disabled:pointer-events-none disabled:opacity-50"
      : "cursor-default",
  );
}
