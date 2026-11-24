"use client";

import type { ReactNode } from "react";
import { Popover, type Placement } from "../primitives/Popover";
import { cn } from "../utils/cn";

export type AgentRosterSeatState = "idle" | "online" | "offline" | "finished" | "degraded";

export interface AgentRosterSeat {
  id: string;
  name: string;
  detail?: string;
  mark?: ReactNode;
  state?: AgentRosterSeatState;
  stateLabel?: string;
  note?: string;
}

export interface AgentRosterProps {
  seats: AgentRosterSeat[];
  label: ReactNode;
  title: string;
  summary?: ReactNode;
  emptyLabel?: ReactNode;
  emptyTone?: "muted" | "warning";
  ariaLabel?: string;
  maxMarks?: number;
  placement?: Placement;
  className?: string;
}

const MARK_TONE: Record<AgentRosterSeatState, string> = {
  idle: "text-cx-fg-2",
  online: "text-cx-fg",
  finished: "text-cx-fg-3",
  offline: "text-cx-fg-4",
  degraded: "text-cx-warning",
};

function SeatMark({ seat }: { seat: AgentRosterSeat }) {
  return (
    <span
      data-slot="agent-roster-mark"
      data-state={seat.state ?? "idle"}
      className={cn("grid size-3.5 shrink-0 place-items-center [&>svg]:size-3.5", MARK_TONE[seat.state ?? "idle"])}
    >
      {seat.mark ?? <span className="text-[10px] font-semibold uppercase leading-none">{seat.name.slice(0, 1)}</span>}
    </span>
  );
}

export function AgentRoster({
  seats,
  label,
  title,
  summary,
  emptyLabel,
  emptyTone = "muted",
  ariaLabel,
  maxMarks = 3,
  placement = "bottom-end",
  className,
}: AgentRosterProps) {
  const visible = seats.slice(0, maxMarks);
  const overflow = seats.length - visible.length;
  const degraded = seats.some((seat) => seat.state === "degraded");
  const warn = degraded || (!seats.length && emptyTone === "warning");

  const trigger = (
    <button
      type="button"
      data-slot="agent-roster"
      data-tone={warn ? "warning" : undefined}
      aria-label={ariaLabel}
      className={cn(
        "cx-press inline-flex h-6 shrink-0 items-center gap-2 rounded-[8px] px-2 text-cx-xs font-medium leading-none outline-none",
        "transition-colors duration-150 ease-out focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
        warn
          ? "text-cx-warning hover:bg-cx-warning-soft data-[state=open]:bg-cx-warning-soft"
          : "text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg-2 data-[state=open]:bg-cx-hover data-[state=open]:text-cx-fg-2",
        className,
      )}
    >
      {seats.length ? (
        <>
          <span>{label}</span>
          <span className="inline-flex items-center gap-1" aria-hidden>
            {visible.map((seat) => <SeatMark key={seat.id} seat={seat} />)}
            {overflow > 0 ? <span className="cx-tabular text-cx-2xs text-cx-fg-3">+{overflow}</span> : null}
          </span>
        </>
      ) : (
        <span>{emptyLabel ?? label}</span>
      )}
    </button>
  );

  return (
    <Popover
      trigger={trigger}
      openOnHover
      delay={150}
      closeDelay={120}
      placement={placement}
      initialFocus="none"
      ariaLabel={title}
      className="min-w-[220px] max-w-[320px]"
    >
      <div className="flex shrink-0 items-baseline justify-between gap-3 px-3 pb-1 pt-2.5">
        <span className="text-cx-xs font-medium text-cx-fg">{title}</span>
        {summary ? <span className="cx-tabular text-cx-2xs text-cx-fg-3">{summary}</span> : null}
      </div>
      {seats.length ? (
        <ul data-slot="agent-roster-list" className="m-0 flex list-none flex-col p-0 pb-1.5">
          {seats.map((seat) => {
            const hot = seat.state === "degraded";
            return (
              <li
                key={seat.id}
                data-state={seat.state ?? "idle"}
                className="grid grid-cols-[14px_minmax(0,1fr)_auto] items-center gap-x-2.5 px-3 py-1.5"
              >
                <SeatMark seat={seat} />
                <span className="flex min-w-0 items-baseline gap-2">
                  <span className="shrink-0 text-cx-xs font-medium text-cx-fg">{seat.name}</span>
                  {seat.detail ? (
                    <span className="truncate font-cx-mono text-cx-2xs text-cx-fg-3" title={seat.detail}>{seat.detail}</span>
                  ) : null}
                </span>
                {seat.stateLabel ? (
                  <span className={cn("whitespace-nowrap text-cx-2xs", hot ? "text-cx-warning" : "text-cx-fg-3")}>{seat.stateLabel}</span>
                ) : <span />}
                {seat.note ? (
                  <span className={cn("col-start-2 col-end-4 mt-1 text-cx-2xs leading-4", hot ? "text-cx-warning" : "text-cx-fg-3")}>{seat.note}</span>
                ) : null}
              </li>
            );
          })}
        </ul>
      ) : (
        <p className={cn("m-0 px-3 pb-3 text-cx-xs leading-5", emptyTone === "warning" ? "text-cx-warning" : "text-cx-fg-3")}>{emptyLabel}</p>
      )}
    </Popover>
  );
}
