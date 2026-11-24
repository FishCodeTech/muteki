"use client";

import { useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@muteki/ui";
import { Popover } from "@muteki/ui";
import type { ConversationScheduledTask } from "@/lib/useConversation";

function scheduleLabel(task: ConversationScheduledTask): string {
  const raw = String(task.schedule || "").trim();
  if (!raw) return task.recurring === false ? "一次性" : "已定时";
  const every = /^every\s+(\d+)\s*(second|minute|hour|day)s?$/i.exec(raw) || /^(\d+)\s*([smhd])$/i.exec(raw);
  if (!every) return raw;
  const unit = every[2].toLowerCase()[0];
  return `每 ${every[1]} ${unit === "s" ? "秒" : unit === "m" ? "分钟" : unit === "h" ? "小时" : "天"}`;
}

function createdLabel(value?: string): string {
  const time = value ? Date.parse(value) : NaN;
  if (!Number.isFinite(time)) return "";
  return new Date(time).toLocaleString(undefined, { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

export function ScheduledTasksPill({
  tasks,
  onAskCancel,
  onAskList,
}: {
  tasks: ConversationScheduledTask[];
  onAskCancel?: (task: ConversationScheduledTask) => void;
  onAskList?: () => void;
}) {
  const [open, setOpen] = useState(false);
  if (!tasks.length) return null;
  const unconfirmed = tasks.some((task) => task.unconfirmed);
  const label = `${tasks.length === 1 ? `定时任务 · ${scheduleLabel(tasks[0])}` : `${tasks.length} 个定时任务`}${unconfirmed ? " · 待确认" : ""}`;
  return (
    <div className="mb-2 flex">
      <Popover
        open={open}
        onOpenChange={setOpen}
        placement="top-start"
        initialFocus="container"
        ariaLabel="Agent 定时任务"
        className="w-[min(420px,calc(100vw-24px))] p-1.5"
        trigger={(
          <button
            type="button"
            data-testid="scheduled-tasks-pill"
            className={cn(
              "cx-press inline-flex h-7 max-w-full items-center gap-1.5 rounded-full ring-1 ring-cx-ring bg-cx-elevated pl-2 pr-1.5 text-cx-xs text-cx-fg-2 shadow-cx-xs outline-none",
              "hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
            )}
          >
            <Icon name="timer" size={13} className="shrink-0 text-cx-accent" />
            <span className="truncate">{label}</span>
            <Icon name="chevronUp" size={13} className="shrink-0 text-cx-fg-4" />
          </button>
        )}
      >
        <p className="px-2 pb-1.5 pt-1 text-cx-xs leading-[18px] text-cx-fg-3">
          Agent 在会话中为自己设定的唤醒。到点后它会自动开启新回合，无需你发送消息。
        </p>
        {unconfirmed ? (
          <div className="mx-1 mb-1.5 flex items-start gap-2 rounded-md bg-cx-bg-subtle px-2 py-1.5 text-cx-xs leading-[18px] text-cx-fg-3" data-testid="scheduled-tasks-unconfirmed">
            <span className="min-w-0 flex-1">
              Agent 会话之后关闭过或服务重启过。有的 Agent 恢复会话时会一并恢复这些任务，有的不会，以之后实际出现的自动唤醒为准。
            </span>
            {onAskList ? (
              <button
                type="button"
                className="shrink-0 rounded px-1.5 py-0.5 text-cx-xs text-cx-fg-3 hover:bg-cx-active hover:text-cx-fg"
                title="在输入框中填入核对请求，由 Agent 列出仍然有效的任务"
                onClick={() => { setOpen(false); onAskList(); }}
              >
                让 Agent 核对
              </button>
            ) : null}
          </div>
        ) : null}
        <ul className="flex max-h-[min(360px,50vh)] flex-col gap-0.5 overflow-auto" data-testid="scheduled-tasks-list">
          {tasks.map((task) => (
            <li key={task.task_id} className="rounded-lg px-2 py-1.5 hover:bg-cx-hover">
              <div className="flex items-center gap-2 text-cx-sm">
                <span className="font-medium text-cx-fg">{scheduleLabel(task)}</span>
                {task.created_at ? <span className="text-cx-xs text-cx-fg-4">创建于 {createdLabel(task.created_at)}</span> : null}
                {onAskCancel ? (
                  <button
                    type="button"
                    className="ml-auto rounded px-1.5 py-0.5 text-cx-xs text-cx-fg-3 hover:bg-cx-active hover:text-cx-fg"
                    title="在输入框中填入取消请求，由 Agent 自己删除该任务"
                    onClick={() => { setOpen(false); onAskCancel(task); }}
                  >
                    让 Agent 取消
                  </button>
                ) : null}
              </div>
              {task.prompt ? (
                <p className="mt-0.5 line-clamp-4 whitespace-pre-wrap break-words text-cx-xs leading-[18px] text-cx-fg-3" title={task.prompt}>
                  {task.prompt}
                </p>
              ) : null}
              <p className="mt-0.5 truncate font-cx-mono text-cx-2xs text-cx-fg-4">{task.task_id}</p>
            </li>
          ))}
        </ul>
      </Popover>
    </div>
  );
}
