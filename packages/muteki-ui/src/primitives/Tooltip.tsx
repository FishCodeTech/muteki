"use client";

import { isValidElement, useEffect, useState, type ReactElement, type ReactNode } from "react";
import { Tooltip as UiTooltip, TooltipContent, TooltipTrigger } from "@ui-sdk/react/tooltip";
import { cn } from "../utils/cn";
import { splitPlacement, type Placement } from "./Popover";
import { Kbd, splitShortcut } from "./Kbd";

let lastTooltipCloseAt = 0;
const WARM_GROUP_WINDOW_MS = 400;

export interface TooltipProps {
  content: ReactNode;
  shortcut?: string | string[];
  placement?: Placement;
  disabled?: boolean;
  delay?: number;
  closeDelay?: number;
  children: ReactElement;
  className?: string;
}

export function Tooltip({
  content,
  shortcut,
  placement = "top",
  disabled,
  delay = 360,
  closeDelay = 80,
  children,
  className,
}: TooltipProps) {
  const [openedWarm, setOpenedWarm] = useState(false);

  useEffect(() => {
    if (!isValidElement(children) || disabled || !content) {
      setOpenedWarm(false);
    }
  }, [children, disabled, content]);

  if (!isValidElement(children) || disabled || !content) return children;
  const { side, align } = splitPlacement(placement);
  const keys = shortcut ? (Array.isArray(shortcut) ? shortcut : splitShortcut(shortcut)) : [];

  return (
    <UiTooltip
      delay={delay}
      closeDelay={closeDelay}
      disabled={disabled}
      onOpenChange={(nextOpen) => {
        if (nextOpen) {
          setOpenedWarm(Date.now() - lastTooltipCloseAt < WARM_GROUP_WINDOW_MS);
        } else {
          lastTooltipCloseAt = Date.now();
        }
      }}
    >
      <TooltipTrigger render={children} />
      <TooltipContent
        side={side}
        align={align}
        sideOffset={7}
        data-warm={openedWarm ? "true" : undefined}
        style={{ transformOrigin: "var(--transform-origin)" }}
        className={cn(
          "cx-portal pointer-events-none z-(--z-tooltip) m-0 flex max-w-[280px] items-center gap-2 rounded-[8px] border border-white/10 dark:border-black/10 px-2.5 py-1 text-cx-xs font-medium leading-[18px]",
          "bg-[color-mix(in_srgb,var(--ink)_94%,var(--page))] text-[var(--page)] shadow-cx-md",
          "muteki-tooltip-anim",
          className,
        )}
      >
        {keys.length ? (
          <>
            <span className="min-w-0">{content}</span>
            <span className="flex shrink-0 items-center gap-0.5">
              {keys.map((key, index) => (
                <Kbd key={`${key}-${index}`} tone="inverse" className="h-[20px] min-w-[20px] px-1">
                  {key}
                </Kbd>
              ))}
            </span>
          </>
        ) : content}
      </TooltipContent>
    </UiTooltip>
  );
}
