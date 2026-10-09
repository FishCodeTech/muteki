"use client";

import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Spinner } from "@/components/chat/ui";

/** Stack of ActivityItem rows, spaced like AgentUI's activity stream. */
export function ActivityRail({ children, className, id }: { children: ReactNode; className?: string; id?: string }) {
  return (
    <div id={id} role="list" className={cn("flex flex-col gap-0.5", className)}>
      {children}
    </div>
  );
}

export type ActivityMarkerTone = "default" | "running" | "danger" | "muted";

/** One activity row: a 1rem glyph column followed by free-form content (AgentUI trace row grid). */
export function ActivityItem({
  icon,
  tone = "default",
  running = false,
  children,
  className,
  testId,
}: {
  icon: IconName;
  tone?: ActivityMarkerTone;
  running?: boolean;
  children: ReactNode;
  className?: string;
  testId?: string;
}) {
  return (
    <div className={cn("grid min-w-0 grid-cols-[1rem_minmax(0,1fr)] gap-x-2.5 px-1.5", className)} data-testid={testId}>
      <span
        aria-hidden
        data-timeline-node
        className={cn(
          "grid h-8 w-4 place-items-center",
          tone === "default" && "text-cx-fg-3/70",
          tone === "muted" && "text-cx-fg-3/55",
          tone === "running" && "text-cx-accent",
          tone === "danger" && "text-cx-danger",
        )}
      >
        {running ? <Spinner size={13} /> : icon === "dot" ? (
          <span className={cn("size-1.5 rounded-full bg-current", tone === "running" && "cx-pulse-dot")} />
        ) : <Icon name={icon} size={15} />}
      </span>
      <div className="min-w-0">{children}</div>
    </div>
  );
}
