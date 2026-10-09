"use client";

import { useRef } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Portal, Shortcut, Spinner, useFloating, useReducedMotion } from "@/components/chat/ui";
import { formatWakeTime } from "@/lib/sidebarInbox";
import type { NavItem } from "./types";

function statusLine(item: NavItem): { icon: React.ReactNode; text: string; tone: string } {
  if (item.running) return { icon: <Spinner size={12} />, text: "Agent 正在运行", tone: "text-cx-accent" };
  if (item.pending === "approval") return { icon: <Icon name="shieldAlert" size={13} />, text: "等待你审批工具调用", tone: "text-cx-warning" };
  if (item.pending === "input") return { icon: <Icon name="messageCircle" size={13} />, text: "Agent 在等待你的回复", tone: "text-cx-accent" };
  if (item.failed) return { icon: <Icon name="circleAlert" size={13} />, text: "上一次运行失败", tone: "text-cx-danger" };
  if (item.woke) return { icon: <Icon name="bell" size={13} />, text: "稍后提醒时间已到", tone: "text-cx-accent" };
  if (item.snoozedUntil) return { icon: <Icon name="clock" size={13} />, text: `将于${formatWakeTime(item.snoozedUntil)}提醒`, tone: "text-cx-fg-3" };
  if (item.settled) return { icon: <Icon name="checkCheck" size={13} />, text: "已归置，有新动态时会回到列表", tone: "text-cx-fg-3" };
  if (item.unread) return { icon: <span className="size-[7px] rounded-full bg-cx-accent" />, text: "有未读更新", tone: "text-cx-accent" };
  return { icon: <Icon name="circleDashed" size={13} />, text: "空闲", tone: "text-cx-fg-3" };
}

export function ThreadHoverCard({ item, anchor }: { item: NavItem | null; anchor: HTMLElement | null }) {
  const anchorRef = useRef<HTMLElement | null>(null);
  anchorRef.current = anchor;
  const open = Boolean(item && anchor);
  const reduced = useReducedMotion();
  const pos = useFloating({ open, anchorRef, placement: "right-start", offset: 10 });
  const status = item ? statusLine(item) : null;
  const updated = item?.updatedAt && Number.isFinite(Date.parse(item.updatedAt))
    ? new Date(item.updatedAt).toLocaleString()
    : "";

  return (
    <Portal>
      <AnimatePresence>
        {open && item && status ? (
          <motion.div
            key="thread-hover-card"
            ref={pos.setFloating}
            role="tooltip"
            data-cx-thread-hover-card=""
            className="pointer-events-none fixed z-[1150] w-[288px] rounded-xl border border-cx-border-subtle bg-cx-overlay p-3 text-[12.5px] leading-5 shadow-cx-pop"
            style={{ top: pos.top, left: pos.left, transformOrigin: pos.origin, visibility: pos.ready ? "visible" : "hidden" }}
            initial={reduced ? { opacity: 0 } : { opacity: 0, x: -4 }}
            animate={{ opacity: 1, x: 0, transition: { duration: 0.14, ease: [0.16, 1, 0.3, 1] } }}
            exit={{ opacity: 0, transition: { duration: 0.08 } }}
          >
            <p className="line-clamp-2 text-[13.5px] font-medium leading-5 text-cx-fg">{item.label}</p>
            {item.projectName ? (
              <p className="mt-1 flex min-w-0 items-center gap-1.5 text-cx-fg-3">
                <Icon name="folder" size={13} className="shrink-0" />
                <span className="shrink-0 text-cx-fg-2">{item.projectName}</span>
                {item.projectPath ? <span className="truncate font-cx-mono text-[11.5px] text-cx-fg-4" title={item.projectPath}>{item.projectPath}</span> : null}
              </p>
            ) : (
              <p className="mt-1 flex items-center gap-1.5 text-cx-fg-4">
                <Icon name="messages" size={13} />
                未归入项目
              </p>
            )}
            <p className={cn("mt-2 flex items-center gap-1.5 font-medium", status.tone)}>
              <span className="grid size-4 place-items-center">{status.icon}</span>
              {status.text}
            </p>
            {item.failed && item.errorText ? (
              <p className="mt-1 line-clamp-3 rounded-md bg-cx-danger-soft px-2 py-1 text-[12px] text-cx-danger">{item.errorText}</p>
            ) : null}
            {item.summary ? <p className="mt-2 line-clamp-3 text-cx-fg-3">{item.summary}</p> : null}
            <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11.5px] text-cx-fg-4">
              {updated ? <span className="flex items-center gap-1"><Icon name="history" size={12} />{updated}</span> : null}
              {item.queueCount ? <span className="flex items-center gap-1"><Icon name="listTodo" size={12} />队列 {item.queueCount} 条</span> : null}
              {item.pinned ? <span className="flex items-center gap-1"><Icon name="pin" size={12} />已置顶</span> : null}
            </div>
            <div className="mt-2.5 flex items-center gap-3 border-t border-cx-border-subtle pt-2 text-[11px] text-cx-fg-4">
              {item.onRenameCommit ? <span>双击重命名</span> : null}
              <span>右键查看更多操作</span>
              <span className="flex items-center gap-1"><Shortcut keys="mod" tone="subtle" />点击多选</span>
            </div>
          </motion.div>
        ) : null}
      </AnimatePresence>
    </Portal>
  );
}
