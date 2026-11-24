"use client";

import {
  useEffect,
  useState,
  type ComponentPropsWithRef,
  type CSSProperties,
  type TransitionEvent,
} from "react";
import { cn } from "../utils/cn";
import { useReducedMotion } from "../utils/reducedMotion";

export interface AgentDisclosureProps extends ComponentPropsWithRef<"div"> {
  open: boolean;
  openHeight?: CSSProperties["height"];
}

export function AgentDisclosure({
  open,
  openHeight = "auto",
  className,
  style,
  children,
  onTransitionEnd,
  ...rest
}: AgentDisclosureProps) {
  const reduced = useReducedMotion();
  const [rendered, setRendered] = useState(open);

  useEffect(() => {
    if (open) {
      setRendered(true);
      return;
    }
    if (reduced) {
      setRendered(false);
      return;
    }
    const timer = window.setTimeout(() => setRendered(false), 220);
    return () => window.clearTimeout(timer);
  }, [open, reduced]);

  const handleTransitionEnd = (event: TransitionEvent<HTMLDivElement>) => {
    onTransitionEnd?.(event);
    if (!open && event.target === event.currentTarget && event.propertyName === "grid-template-rows") {
      setRendered(false);
    }
  };

  return (
    <div
      data-slot="agent-disclosure"
      data-state={open ? "open" : "closed"}
      aria-hidden={!open || undefined}
      inert={!open || undefined}
      onTransitionEnd={handleTransitionEnd}
      className={cn(
        "grid transition-[grid-template-rows,opacity] duration-200 ease-cx-out",
        reduced && "duration-0",
        open ? "grid-rows-[1fr] opacity-100" : "grid-rows-[0fr] opacity-0",
        className,
      )}
      style={style}
      {...rest}
    >
      <div
        className={cn("min-h-0 overflow-hidden", openHeight !== "auto" && "overflow-y-auto")}
        style={openHeight !== "auto" ? { maxHeight: openHeight } : undefined}
      >
        {rendered || open ? children : null}
      </div>
    </div>
  );
}
