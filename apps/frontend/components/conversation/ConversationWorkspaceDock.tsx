"use client";


import React, { useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@muteki/ui";
import { EmptyState, IconButton, ScrollArea, SegmentedControl, Spinner } from "@muteki/ui";
import type { ConversationEvent } from "@/lib/useConversation";
import type { TransitionPhase } from "@/lib/useTransitionState";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";
import { collectConversationTools, type ConversationToolRecord } from "./conversationEventViews";
import { formatToolDuration, toolArgSummary, toolKind, toolKindIcon, toolLabel } from "@/components/chat/timeline/toolPresentation";
import { toolPayload } from "@/components/chat/surfaces/shared";

type LogFilter = "all" | "running" | "failed";

function StatusGlyph({ status }: { status: ConversationToolRecord["status"] }) {
  switch (status) {
    case "running":
      return <Spinner size={12} className="text-cx-accent" />;
    case "completed":
      return <Icon name="check" size={13} className="text-cx-fg-4" />;
    case "failed":
      return <Icon name="x" size={13} className="text-cx-danger" />;
    case "cancelled":
      return <Icon name="minus" size={13} className="text-cx-fg-4" />;
    case "declined":
      return <Icon name="minus" size={13} className="text-cx-warning" />;
    case "pending":
      return <span aria-hidden="true" className="grid size-3 place-items-center"><span className="size-1.5 rounded-full bg-cx-fg-4/70" /></span>;
    default: {
      const exhaustive: never = status;
      return exhaustive;
    }
  }
}

export function ConversationBottomPanel({
  events,
  onClose,
  onOpenDetails,
  transitionPhase = "open",
  instant = false,
}: {
  events: ConversationEvent[];
  onClose: () => void;
  onOpenDetails: (payload: DrawerDetailPayload) => void;
  transitionPhase?: TransitionPhase;
  instant?: boolean;
}) {
  const tools = useMemo(() => collectConversationTools(events), [events]);
  const [filter, setFilter] = useState<LogFilter>("all");
  const running = tools.filter((tool) => tool.status === "running").length;
  const failed = tools.filter((tool) => tool.status === "failed").length;
  const visible = useMemo(() => {
    const rows = filter === "all" ? tools : tools.filter((tool) => tool.status === filter);
    return [...rows].reverse();
  }, [filter, tools]);
  const open = transitionPhase === "open" || (instant && transitionPhase === "opening");

  return (
    <section
      data-instant={instant ? "true" : undefined}
      className={cn(
        "flex h-[min(320px,42vh)] shrink-0 flex-col border-t border-cx-border bg-cx-bg shadow-[0_-10px_24px_-18px_hsl(var(--cx-shadow-color)/0.5)]",
        instant ? "transition-none" : "transition-[transform,opacity] duration-[200ms] ease-cx-drawer",
        open ? "translate-y-0 opacity-100" : instant ? "opacity-0" : "translate-y-3 opacity-0",
      )}
      aria-label="Agent 执行日志"
      aria-hidden={!open}
      data-testid="conversation-execution-log"
    >
      <header className="flex h-10 shrink-0 items-center gap-2 border-b border-cx-border-subtle pl-3 pr-1.5">
        <Icon name="panelBottom" size={14} className="text-cx-fg-3" />
        <strong className="text-cx-sm font-semibold text-cx-fg">执行日志</strong>
        <span className="cx-tabular text-cx-xs text-cx-fg-4">{running ? `${running} 项执行中` : `${tools.length} 条`}</span>
        <SegmentedControl<LogFilter>
          size="xs"
          value={filter}
          onChange={setFilter}
          ariaLabel="日志筛选"
          className="ml-2"
          options={[
            { value: "all", label: "全部" },
            { value: "running", label: `运行中${running ? ` ${running}` : ""}`, disabled: !running },
            { value: "failed", label: `失败${failed ? ` ${failed}` : ""}`, disabled: !failed },
          ]}
        />
        <IconButton icon="x" label="关闭执行日志" shortcut="mod+j" size="sm" className="ml-auto" onClick={onClose} />
      </header>
      {visible.length ? (
        <ScrollArea className="flex-1" role="log" aria-live="polite">
          <ul className="m-0 flex list-none flex-col p-1.5">
            {visible.map((tool) => {
              const kind = toolKind(tool);
              const summary = toolArgSummary(tool, kind);
              const duration = formatToolDuration(tool.durationMs);
              const detail = tool.error || "";
              return (
                <li key={tool.id}>
                  <button
                    type="button"
                    onClick={() => onOpenDetails(toolPayload(tool))}
                    className="group flex w-full items-center gap-2.5 rounded-lg px-2 py-1.5 text-left transition-colors hover:bg-cx-hover"
                  >
                    <Icon name={toolKindIcon(kind)} size={14} className={cn("shrink-0", tool.status === "failed" ? "text-cx-danger" : "text-cx-fg-4")} />
                    <span className={cn("shrink-0 text-cx-sm font-medium", tool.status === "failed" ? "text-cx-danger" : "text-cx-fg-2")}>{toolLabel(tool, kind)}</span>
                    <span className="min-w-0 flex-1 truncate font-cx-mono text-cx-xs text-cx-fg-3">{detail || summary}</span>
                    {duration ? <span className="cx-tabular shrink-0 text-cx-xs text-cx-fg-4">{duration}</span> : null}
                    <span className="grid size-4 shrink-0 place-items-center"><StatusGlyph status={tool.status} /></span>
                  </button>
                </li>
              );
            })}
          </ul>
        </ScrollArea>
      ) : (
        <EmptyState compact icon="panelBottom" title={tools.length ? "没有符合筛选的记录" : "暂无执行记录"} description={tools.length ? undefined : "工具开始执行后，输入、结果和异常会显示在这里。"} />
      )}
    </section>
  );
}
