"use client";

import { memo, useEffect, useRef, useState, type CSSProperties, type KeyboardEvent, type MouseEvent, type PointerEvent, type ReactNode, type RefObject } from "react";
import { motion } from "motion/react";
import { useLang } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@muteki/ui";
import { EASE_OUT, Shortcut, Skeleton, Spinner, StatusDot, Tooltip, splitShortcut } from "@muteki/ui";
import { EngineLogo } from "@/components/EngineLogo";
import { formatCompactRelative } from "./time";
import type { NavItem, NavSection, SidebarBodyHit } from "./types";

export const ROW_SELECTOR = "[data-cx-nav-row]";

export interface RowDragHandlers {
  onPointerDown: (event: PointerEvent<HTMLElement>) => void;
  onPointerMove: (event: PointerEvent<HTMLElement>) => void;
  onPointerUp: (event: PointerEvent<HTMLElement>) => void;
  onPointerCancel: () => void;
}

export function highlightMatch(text: string, query: string): ReactNode {
  const q = query.trim();
  if (!q) return text;
  const index = text.toLowerCase().indexOf(q.toLowerCase());
  if (index < 0) return text;
  return (
    <>
      {text.slice(0, index)}
      <mark className="rounded-[3px] bg-cx-warning-soft px-px text-cx-fg">{text.slice(index, index + q.length)}</mark>
      {text.slice(index + q.length)}
    </>
  );
}

type SlotTone = "accent" | "warning" | "danger" | "muted";

const SLOT_TONE: Record<SlotTone, string> = {
  accent: "text-cx-accent",
  warning: "text-cx-warning",
  danger: "text-cx-danger",
  muted: "text-cx-fg-4",
};

function SlotLabel({ tone, icon, children, title }: { tone: SlotTone; icon: ReactNode; children: ReactNode; title?: string }) {
  return (
    <span className={cn("flex h-5 items-center gap-1 whitespace-nowrap text-cx-xs font-medium leading-none", SLOT_TONE[tone])} title={title}>
      {icon}
      <span>{children}</span>
    </span>
  );
}

function compactWake(until: number, nowMs: number): string {
  const date = new Date(until);
  const now = new Date(nowMs);
  const dayStart = (value: Date) => new Date(value.getFullYear(), value.getMonth(), value.getDate()).getTime();
  const days = Math.round((dayStart(date) - dayStart(now)) / 86_400_000);
  if (days <= 0) return `${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
  if (days === 1) return "明天";
  if (days < 7) return ["周日", "周一", "周二", "周三", "周四", "周五", "周六"][date.getDay()];
  return `${date.getMonth() + 1}/${date.getDate()}`;
}

type SlotKind = "label" | "time" | "none";

function slotKindOf(item: NavItem): SlotKind {
  if (item.running || item.pending || item.failed || item.agentsActive || item.woke || item.snoozedUntil) return "label";
  return item.updatedAt ? "time" : "none";
}

export function rowClockLabel(item: NavItem, nowMs: number): string {
  if (item.running || item.pending || item.failed || item.agentsActive || item.woke) return "";
  if (item.snoozedUntil) return compactWake(item.snoozedUntil, nowMs);
  return formatCompactRelative(item.updatedAt, nowMs);
}

function StatusSlot({ item, clockLabel, exactTime }: { item: NavItem; clockLabel: string; exactTime?: string }) {
  const agentsNote = item.agentsActive ? `${item.agentsActive} 个子代理进行中` : "";
  if (item.pending === "approval") {
    return <SlotLabel tone="warning" icon={<Icon name="shieldAlert" size={12} />} title={item.pendingNote || "等待你审批工具调用"}>待审批</SlotLabel>;
  }
  if (item.pending === "input") {
    return <SlotLabel tone="accent" icon={<Icon name="messageCircle" size={12} />} title={item.pendingNote || "Agent 在等待你的回复"}>待回复</SlotLabel>;
  }
  if (item.running) {
    return <SlotLabel tone="accent" icon={<Spinner size={11} />} title={agentsNote ? `Agent 正在运行 · ${agentsNote}` : "Agent 正在运行"}>运行中</SlotLabel>;
  }
  if (item.failed) {
    return <SlotLabel tone="danger" icon={<Icon name="circleAlert" size={12} />} title={item.errorText || "对话异常"}>失败</SlotLabel>;
  }
  if (agentsNote) {
    return <SlotLabel tone="muted" icon={<Icon name="bot" size={12} />} title={agentsNote}>委派中</SlotLabel>;
  }
  if (item.woke) {
    return (
      <span className="flex h-[18px] items-center gap-1 rounded-full bg-cx-accent-soft px-1.5 text-cx-2xs font-medium leading-none text-cx-accent" title="稍后提醒时间已到">
        <Icon name="bell" size={11} />
        已唤醒
      </span>
    );
  }
  if (item.snoozedUntil) {
    return (
      <SlotLabel tone="muted" icon={<Icon name="clock" size={12} />} title={`将于 ${new Date(item.snoozedUntil).toLocaleString()} 提醒`}>
        {clockLabel}
      </SlotLabel>
    );
  }
  const time = clockLabel;
  return (
    <span className="flex items-center gap-1.5">
      {item.unread ? <StatusDot tone="accent" label="有未读更新" className="size-[7px]" /> : null}
      {time ? <span className="cx-tabular text-cx-xs leading-none text-cx-fg-4" title={exactTime}>{time}</span> : null}
    </span>
  );
}

function RowAction({ icon, label, shortcut, onClick, expanded }: {
  icon: IconName;
  label: string;
  shortcut?: string;
  onClick: (anchor: HTMLElement) => void;
  expanded?: boolean;
}) {
  return (
    <Tooltip content={label} shortcut={shortcut ? splitShortcut(shortcut) : undefined} placement="top">
      <button
        type="button"
        tabIndex={-1}
        aria-label={label}
        aria-haspopup={expanded === undefined ? undefined : "menu"}
        aria-expanded={expanded}
        onClick={(event) => {
          event.preventDefault();
          event.stopPropagation();
          onClick(event.currentTarget);
        }}
        className="cx-press grid size-6 place-items-center rounded-lg text-cx-fg-3 outline-none hover:bg-cx-active hover:text-cx-fg aria-expanded:bg-cx-active aria-expanded:text-cx-fg"
      >
        <Icon name={icon} size={14} />
      </button>
    </Tooltip>
  );
}

function InlineRename({ item, indent, onDone }: { item: NavItem; indent: boolean; onDone: () => void }) {
  const [value, setValue] = useState(item.label);
  const inputRef = useRef<HTMLInputElement>(null);
  const finishedRef = useRef(false);
  useEffect(() => {
    const input = inputRef.current;
    if (!input) return;
    input.focus();
    input.select();
  }, []);
  const finish = (commit: boolean) => {
    if (finishedRef.current) return;
    finishedRef.current = true;
    const title = value.trim();
    if (commit && title && title !== item.label) item.onRenameCommit?.(title);
    onDone();
  };
  return (
    <div className={cn("flex h-9 items-center rounded-xl bg-cx-active pr-1.5", indent ? "pl-[26px]" : "pl-1.5")}>
      <input
        ref={inputRef}
        value={value}
        aria-label="对话标题"
        maxLength={200}
        onChange={(event) => setValue(event.target.value)}
        onKeyDown={(event) => {
          event.stopPropagation();
          if (event.nativeEvent.isComposing) return;
          if (event.key === "Enter") { event.preventDefault(); finish(true); }
          else if (event.key === "Escape") { event.preventDefault(); finish(false); }
        }}
        onBlur={() => finish(true)}
        className="h-7 min-w-0 flex-1 rounded-lg border border-[var(--cx-focus)] bg-cx-elevated px-1.5 text-cx-base text-cx-fg outline-none"
      />
    </div>
  );
}

export interface ThreadRowProps {
  item: NavItem;
  sectionKey: string;
  index: number;
  active: boolean;
  variant?: "default" | "activity";
  indent?: boolean;
  clockLabel: string;
  highlight?: string;
  menuOpen: boolean;
  dragging: boolean;
  dragOver: boolean;
  reduced: boolean;
  suppressEnter?: boolean;
  selected?: boolean;
  jumpHint?: number;
  renaming?: boolean;
  didDragRef: RefObject<boolean>;
  drag?: RowDragHandlers;
  onSelect: (id: string, event: MouseEvent<HTMLElement>) => void;
  onOpenMenu: (item: NavItem, anchor: HTMLElement, sectionKey: string) => void;
  onContextMenu: (event: MouseEvent<HTMLElement>, item: NavItem, sectionKey: string) => void;
  onKeyDown: (event: KeyboardEvent<HTMLElement>, item: NavItem, sectionKey: string) => void;
  onStartRename?: (item: NavItem) => void;
  onRenameDone?: () => void;
  onHover?: (item: NavItem, element: HTMLElement | null) => void;
}

const SLOT_PADDING: Record<SlotKind, number> = { label: 72, time: 46, none: 36 };
const ACTIONS_PADDING = 86;

export const ThreadRow = memo(function ThreadRow({
  item,
  sectionKey,
  index,
  active,
  variant = "default",
  indent = false,
  clockLabel,
  highlight = "",
  menuOpen,
  dragging,
  dragOver,
  reduced,
  suppressEnter = false,
  selected = false,
  jumpHint,
  renaming = false,
  didDragRef,
  drag,
  onSelect,
  onOpenMenu,
  onContextMenu,
  onKeyDown,
  onStartRename,
  onRenameDone,
  onHover,
}: ThreadRowProps) {
  const activity = variant === "activity";
  const receded = item.settled || Boolean(item.snoozedUntil);
  const exactTime = item.updatedAt && Number.isFinite(Date.parse(item.updatedAt))
    ? new Date(item.updatedAt).toLocaleString()
    : undefined;
  const slotKind = slotKindOf(item);
  const hinting = jumpHint !== undefined;
  const engaged = menuOpen;

  return (
    <motion.div
      initial={reduced || suppressEnter ? false : { opacity: 0 }}
      animate={{ opacity: dragging ? 0.45 : 1 }}
      exit={
        suppressEnter
          ? { opacity: 0, transition: { duration: 0 } }
          : reduced
            ? { opacity: 0, transition: { duration: 0.08 } }
            : {
                opacity: 0,
                height: 0,
                overflow: "hidden",
                transition: {
                  opacity: { duration: 0.12, ease: EASE_OUT },
                  height: { duration: 0.2, ease: EASE_OUT },
                },
              }
      }
      transition={{ duration: reduced ? 0 : 0.12, ease: EASE_OUT }}
      className={cn(
        "group/row relative",
        dragOver && "before:pointer-events-none before:absolute before:inset-x-2 before:-top-px before:z-10 before:h-0.5 before:rounded-full before:bg-cx-accent",
      )}
      style={{
        "--cx-row-slot": `${hinting ? 44 : SLOT_PADDING[slotKind]}px`,
        "--cx-row-actions": `${hinting ? 44 : ACTIONS_PADDING}px`,
      } as CSSProperties}
      data-sidebar-section={sectionKey}
      data-sidebar-item={item.id}
      data-sidebar-index={index}
      data-selected={active ? "true" : undefined}
      data-multi-selected={selected ? "true" : undefined}
      data-menu-open={engaged ? "true" : undefined}
      data-hinting={hinting ? "true" : undefined}
      onContextMenu={(event) => onContextMenu(event, item, sectionKey)}
      onPointerEnter={(event) => { if (event.pointerType === "mouse") onHover?.(item, event.currentTarget); }}
      onPointerLeave={() => onHover?.(item, null)}
    >
      {renaming ? (
        <InlineRename item={item} indent={indent} onDone={() => onRenameDone?.()} />
      ) : (
        <a
          href={item.href || "#"}
          draggable={false}
          data-cx-nav-row=""
          data-cx-openable=""
          aria-current={active ? "page" : undefined}
          aria-keyshortcuts="Shift+F10"
          onClick={(event) => {
            if (didDragRef.current) {
              event.preventDefault();
              return;
            }
            if (event.button !== 0) return;
            event.preventDefault();
            onSelect(item.id, event);
          }}
          onAuxClick={(event) => {
            if (event.button === 1 && item.href) return;
            event.preventDefault();
          }}
          onDoubleClick={(event) => {
            if (!item.onRenameCommit || event.metaKey || event.ctrlKey || event.shiftKey) return;
            event.preventDefault();
            onStartRename?.(item);
          }}
          onKeyDown={(event) => onKeyDown(event, item, sectionKey)}
          onPointerDown={drag?.onPointerDown}
          onPointerMove={drag?.onPointerMove}
          onPointerUp={drag?.onPointerUp}
          onPointerCancel={drag?.onPointerCancel}
          className={cn(
            "flex w-full min-w-0 select-none items-center rounded-xl text-cx-base leading-5 no-underline outline-none",
            "pr-[var(--cx-row-slot)] group-hover/row:pr-[var(--cx-row-actions)] group-has-[:focus-visible]/row:pr-[var(--cx-row-actions)] group-data-[menu-open=true]/row:pr-[var(--cx-row-actions)]",
            "cx-press text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg",
            "focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
            "group-data-[menu-open=true]/row:bg-cx-hover",
            activity ? "min-h-12 py-2" : "h-9",
            indent ? "pl-[30px]" : "pl-2.5",
            receded && !active && "text-cx-fg-3",
            active && "bg-cx-active font-medium text-cx-fg hover:bg-cx-active group-data-[menu-open=true]/row:bg-cx-active",
            selected && "bg-cx-accent-soft text-cx-fg hover:bg-cx-accent-soft group-data-[menu-open=true]/row:bg-cx-accent-soft",
          )}
        >
          {item.engine === undefined ? null : (
            <span aria-hidden="true" className={cn("mr-2 grid size-4 shrink-0 place-items-center", activity && "self-start mt-0.5")}>
              {item.engine ? <EngineLogo engine={item.engine} size={15} /> : null}
            </span>
          )}
          {activity ? (
            <span className="flex min-w-0 flex-1 flex-col">
              <span className="cx-sb-fade min-w-0 overflow-hidden whitespace-nowrap">{highlightMatch(item.label, highlight)}</span>
              {item.pendingNote ? (
                <span className="flex min-w-0 items-center gap-1 text-cx-xs font-normal leading-4 text-cx-warning">
                  <Icon name={item.pending === "input" ? "messageCircle" : "shieldAlert"} size={12} className="shrink-0" />
                  <span className="truncate">{item.pendingNote}</span>
                </span>
              ) : item.subtitle ? (
                <span className="flex min-w-0 items-center gap-1 text-cx-xs font-normal leading-4 text-cx-fg-4">
                  <Icon name="folder" size={12} className="shrink-0" />
                  <span className="truncate">{item.subtitle}</span>
                </span>
              ) : null}
            </span>
          ) : (
            <span className="cx-sb-fade min-w-0 flex-1 overflow-hidden whitespace-nowrap">{highlightMatch(item.label, highlight)}</span>
          )}
        </a>
      )}
      {renaming ? null : (
        <div className="pointer-events-none absolute inset-y-0 right-1 flex items-center">
          {hinting ? (
            <span className="pr-1"><Shortcut keys={`mod+${jumpHint}`} tone="default" /></span>
          ) : (
            <span className="relative flex h-6 items-center justify-end">
              <span
                className={cn(
                  "flex items-center pr-1 transition-opacity duration-100",
                  "group-hover/row:opacity-0 group-has-[:focus-visible]/row:opacity-0 group-data-[menu-open=true]/row:opacity-0",
                )}
              >
                <StatusSlot item={item} clockLabel={clockLabel} exactTime={exactTime} />
              </span>
              <span
                className={cn(
                  "absolute right-0 flex items-center gap-0.5 opacity-0 transition-opacity duration-100",
                  "group-hover/row:opacity-100 group-has-[:focus-visible]/row:opacity-100 group-data-[menu-open=true]/row:opacity-100",
                  "[&>*]:pointer-events-none group-hover/row:[&>*]:pointer-events-auto group-has-[:focus-visible]/row:[&>*]:pointer-events-auto group-data-[menu-open=true]/row:[&>*]:pointer-events-auto",
                )}
              >
                {item.settled && item.onSettle ? (
                  <RowAction icon="undo" label="移回列表" shortcut="mod+shift+s" onClick={() => item.onSettle?.()} />
                ) : item.snoozedUntil && item.onSnooze ? (
                  <RowAction icon="bell" label="立即唤醒" onClick={() => item.onSnooze?.(null)} />
                ) : (
                  <>
                    {item.onPin ? (
                      <RowAction icon={item.pinned ? "pinOff" : "pin"} label={item.pinned ? "取消置顶" : "置顶"} shortcut="mod+shift+p" onClick={() => item.onPin?.()} />
                    ) : null}
                    {item.onSettle ? (
                      <RowAction icon="check" label="归置" shortcut="mod+shift+s" onClick={() => item.onSettle?.()} />
                    ) : null}
                  </>
                )}
                <button
                  type="button"
                  tabIndex={-1}
                  data-cx-row-more=""
                  aria-label={`${item.label} 的更多操作`}
                  aria-haspopup="menu"
                  aria-expanded={menuOpen}
                  onClick={(event) => {
                    event.preventDefault();
                    event.stopPropagation();
                    onOpenMenu(item, event.currentTarget, sectionKey);
                  }}
                  className={cn(
                    "cx-press grid size-6 place-items-center rounded-lg text-cx-fg-3 outline-none",
                    "hover:bg-cx-active hover:text-cx-fg aria-expanded:bg-cx-active aria-expanded:text-cx-fg",
                  )}
                >
                  <Icon name="more" size={15} />
                </button>
              </span>
            </span>
          )}
        </div>
      )}
    </motion.div>
  );
});

export function SectionLabel({
  title,
  collapsed,
  onToggle,
  controlsId,
  actions,
  className,
}: {
  title: ReactNode;
  collapsed: boolean;
  onToggle: () => void;
  controlsId: string;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <div data-cx-section-label="" className={cn("group/label flex h-9 items-center justify-between gap-1 pl-2.5 pr-1", className)}>
      <button
        type="button"
        data-cx-nav-row=""
        aria-expanded={!collapsed}
        aria-controls={controlsId}
        onClick={onToggle}
        className="-ml-1 flex h-6 min-w-0 items-center gap-1 rounded-md px-1 text-cx-xs font-medium text-cx-fg-3 outline-none transition-colors hover:text-cx-fg-2 focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
      >
        <span className="truncate">{title}</span>
        <Icon
          name="chevronRight"
          size={12}
          className={cn(
            "shrink-0 transition-[transform,opacity] duration-200 ease-cx-out",
            collapsed ? "rotate-0 opacity-100" : "rotate-90 opacity-0 group-hover/label:opacity-100 group-has-[:focus-visible]/label:opacity-100",
          )}
        />
      </button>
      {actions ? (
        <span className="flex items-center gap-0.5 opacity-0 transition-opacity duration-150 group-hover/label:opacity-100 group-has-[:focus-visible]/label:opacity-100 has-[[aria-expanded=true]]:opacity-100">
          {actions}
        </span>
      ) : null}
    </div>
  );
}

export const FolderHeader = memo(function FolderHeader({
  section,
  sectionKey,
  collapsed,
  count,
  menuOpen,
  dragOver,
  controlsId,
  didDragRef,
  drag,
  icon = "folder",
  openIcon = "folderOpen",
  newThreadLabel = "在此项目中新建对话",
  onToggle,
  onOpenMenu,
  onContextMenu,
  onKeyDown,
}: {
  section: NavSection;
  sectionKey: string;
  collapsed: boolean;
  count: number;
  menuOpen: boolean;
  dragOver: boolean;
  controlsId: string;
  didDragRef: RefObject<boolean>;
  drag?: RowDragHandlers;
  icon?: IconName;
  openIcon?: IconName;
  newThreadLabel?: string;
  onToggle: (sectionKey: string) => void;
  onOpenMenu: (anchor: HTMLElement, sectionKey: string) => void;
  onContextMenu: (event: MouseEvent<HTMLElement>, sectionKey: string) => void;
  onKeyDown: (event: KeyboardEvent<HTMLElement>, sectionKey: string, collapsed: boolean) => void;
}) {
  return (
    <div
      className={cn(
        "group/folder relative",
        dragOver && "before:pointer-events-none before:absolute before:inset-x-2 before:-top-px before:z-10 before:h-0.5 before:rounded-full before:bg-cx-accent",
      )}
      data-cx-folder-header=""
      data-sidebar-section={sectionKey}
      data-sidebar-label={section.title}
      data-menu-open={menuOpen ? "true" : undefined}
      onContextMenu={(event) => onContextMenu(event, sectionKey)}
    >
      <button
        type="button"
        data-cx-nav-row=""
        aria-expanded={!collapsed}
        aria-controls={controlsId}
        aria-keyshortcuts="Shift+F10"
        title={[section.title, section.subtitle].filter(Boolean).join(" · ")}
        aria-label={[section.title, section.subtitle].filter(Boolean).join(" · ")}
        onClick={() => {
          if (didDragRef.current) return;
          onToggle(sectionKey);
        }}
        onKeyDown={(event) => onKeyDown(event, sectionKey, collapsed)}
        onPointerDown={drag?.onPointerDown}
        onPointerMove={drag?.onPointerMove}
        onPointerUp={drag?.onPointerUp}
        onPointerCancel={drag?.onPointerCancel}
        className={cn(
          "flex min-h-9 w-full min-w-0 select-none items-center gap-2 rounded-xl pl-2.5 pr-[62px] text-left text-cx-base leading-5 text-cx-fg-2 outline-none",
          "cx-press hover:bg-cx-hover hover:text-cx-fg group-data-[menu-open=true]/folder:bg-cx-hover",
          "focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
        )}
      >
        <Icon name={collapsed ? icon : openIcon} size={15} className="shrink-0 text-cx-fg-3" />
        <span className="min-w-0 flex-1 py-1">
          <span className="block truncate">{section.title}</span>
        </span>
      </button>
      <div className="pointer-events-none absolute inset-y-0 right-1 flex items-center gap-0.5">
        {section.running ? (
          <span className="grid size-5 place-items-center text-cx-accent" title="项目中有任务正在运行">
            <Spinner size={12} />
            <span className="sr-only">项目中有任务正在运行</span>
          </span>
        ) : null}
        <span className="relative flex h-6 items-center">
          <span className="cx-tabular min-w-6 pr-1.5 text-right text-cx-xs text-cx-fg-4 transition-opacity duration-100 group-hover/folder:opacity-0 group-has-[:focus-visible]/folder:opacity-0 group-data-[menu-open=true]/folder:opacity-0">
            {count || ""}
          </span>
          <span className="pointer-events-auto absolute right-0 flex items-center gap-0.5 opacity-0 transition-opacity duration-100 group-hover/folder:opacity-100 group-has-[:focus-visible]/folder:opacity-100 group-data-[menu-open=true]/folder:opacity-100">
            {section.onNewChat ? (
              <button
                type="button"
                tabIndex={-1}
                aria-label={`在 ${section.title} 中${newThreadLabel}`}
                title={newThreadLabel}
                onClick={(event) => {
                  event.stopPropagation();
                  section.onNewChat?.();
                }}
                className="cx-press grid size-6 place-items-center rounded-lg text-cx-fg-3 hover:bg-cx-active hover:text-cx-fg"
              >
                <Icon name="edit" size={14} />
              </button>
            ) : null}
            <button
              type="button"
              tabIndex={-1}
              data-cx-row-more=""
              aria-label={`${section.title} 菜单`}
              aria-haspopup="menu"
              aria-expanded={menuOpen}
              onClick={(event) => {
                event.stopPropagation();
                onOpenMenu(event.currentTarget, sectionKey);
              }}
              className="cx-press grid size-6 place-items-center rounded-lg text-cx-fg-3 hover:bg-cx-active hover:text-cx-fg"
            >
              <Icon name="more" size={15} />
            </button>
          </span>
        </span>
      </div>
    </div>
  );
});

function roleLabel(role: string): string {
  if (role === "user") return "你";
  if (role === "assistant") return "Agent";
  if (role === "system") return "系统";
  return role || "消息";
}

function renderSnippet(snippet: string): ReactNode[] {
  const parts: ReactNode[] = [];
  const re = /«([^»]*)»/g;
  let last = 0;
  let match: RegExpExecArray | null;
  let key = 0;
  while ((match = re.exec(snippet))) {
    if (match.index > last) parts.push(snippet.slice(last, match.index));
    parts.push(
      <mark key={key} className="rounded-[3px] bg-cx-warning-soft px-px text-cx-fg">{match[1]}</mark>,
    );
    key += 1;
    last = match.index + match[0].length;
  }
  if (last < snippet.length) parts.push(snippet.slice(last));
  return parts;
}

export function SearchHits({
  hits,
  loading,
  loadingMore,
  error,
  hasMore,
  onLoadMore,
  onRetry,
  activeId,
  includeSuperseded,
  onIncludeSupersededChange,
  onSelect,
}: {
  hits: SidebarBodyHit[];
  loading: boolean;
  loadingMore: boolean;
  error: string;
  hasMore: boolean;
  onLoadMore?: () => void;
  onRetry?: () => void;
  activeId?: string;
  includeSuperseded: boolean;
  onIncludeSupersededChange?: (value: boolean) => void;
  onSelect: (hit: SidebarBodyHit) => void;
}) {
  const { lang } = useLang();
  const english = lang === "en";
  return (
    <section aria-label="消息正文命中" className="mb-2">
      <div className="flex h-8 items-center justify-between gap-2 pl-2.5 pr-1">
        <span className="text-cx-xs font-medium text-cx-fg-4">消息</span>
        {onIncludeSupersededChange ? (
          <button
            type="button"
            aria-pressed={includeSuperseded}
            onClick={() => onIncludeSupersededChange(!includeSuperseded)}
            className={cn(
              "cx-press flex h-6 items-center gap-1 rounded-md px-1.5 text-cx-xs outline-none",
              "focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
              includeSuperseded ? "bg-cx-selected text-cx-fg" : "text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg-2",
            )}
          >
            <Icon name={includeSuperseded ? "check" : "history"} size={12} />
            含已替代历史
          </button>
        ) : null}
      </div>
      {loading ? (
        <div role="status" aria-label="正在搜索正文" className="flex flex-col gap-2 px-2.5 py-1.5">
          <Skeleton className="h-3 w-[70%]" />
          <Skeleton className="h-2.5 w-[90%]" />
          <Skeleton className="mt-1 h-3 w-[56%]" />
          <Skeleton className="h-2.5 w-[82%]" />
        </div>
      ) : hits.length ? (
        <div className="flex flex-col gap-px">
          {hits.map((hit) => (
            <button
              key={`${hit.threadId}:${hit.messageId}`}
              type="button"
              data-cx-nav-row=""
              data-cx-openable=""
              aria-current={hit.threadId === activeId ? "true" : undefined}
              onClick={() => onSelect(hit)}
              className={cn(
                "cx-press flex w-full min-w-0 flex-col gap-0.5 rounded-lg px-2.5 py-1.5 text-left outline-none",
                "hover:bg-cx-hover focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
                hit.threadId === activeId && "bg-cx-active",
              )}
            >
              <span className="flex min-w-0 items-center gap-1.5">
                <span className="min-w-0 flex-1 truncate text-cx-sm font-medium leading-5 text-cx-fg">{hit.title || "未命名对话"}</span>
                {hit.archived ? <span className="shrink-0 rounded px-1 text-cx-xs leading-4 text-cx-fg-4 ring-1 ring-cx-border ring-inset">已归档</span> : null}
                {hit.superseded ? <span className="shrink-0 rounded px-1 text-cx-xs leading-4 text-cx-fg-4 ring-1 ring-cx-border ring-inset">已替代</span> : null}
                <span className="shrink-0 text-cx-xs text-cx-fg-4">{roleLabel(hit.role)}</span>
              </span>
              <span className="line-clamp-2 text-cx-xs leading-[18px] text-cx-fg-3">{renderSnippet(String(hit.snippet || ""))}</span>
            </button>
          ))}
        </div>
      ) : !error ? (
        <p className="px-2.5 py-1.5 text-cx-sm text-cx-fg-4">{english ? "No matching message text" : "没有匹配的消息正文"}</p>
      ) : null}
      {error ? <div role="alert" className="mx-2.5 my-2 rounded-md border border-cx-border p-2 text-cx-xs text-cx-warning">
        <p>{english ? "Message search did not finish." : "正文搜索未完成。"}</p>
        <details><summary>{english ? "Error details" : "错误详情"}</summary><pre className="mt-1 whitespace-pre-wrap break-all font-cx-mono text-cx-xs">{error}</pre></details>
        {onRetry ? <button type="button" onClick={onRetry} disabled={loading || loadingMore} className="mt-1 rounded px-2 py-1 text-cx-accent focus-visible:outline-2">{english ? "Retry" : "重试"}</button> : null}
      </div> : null}
      {!loading && hits.length ? <p role="status" className="px-2.5 pt-1 text-cx-xs text-cx-fg-4">{english ? `${hits.length} loaded${hasMore ? "; more results available" : "; all results loaded"}` : `已加载 ${hits.length} 条${hasMore ? "，还有更多结果" : "，已加载全部结果"}`}</p> : null}
      {!error && hasMore && onLoadMore ? <button type="button" onClick={onLoadMore} disabled={loadingMore} className="mx-2.5 mt-1 rounded-md px-2 py-1 text-cx-xs text-cx-accent hover:bg-cx-hover focus-visible:outline-2">{loadingMore ? (english ? "Loading…" : "加载中…") : (english ? "Load more results" : "加载更多结果")}</button> : null}
    </section>
  );
}

const SKELETON_WIDTHS = ["w-[72%]", "w-[54%]", "w-[80%]", "w-[62%]", "w-[46%]", "w-[68%]", "w-[58%]"];

export function SkeletonRows() {
  return (
    <div role="status" aria-label="正在加载对话" className="flex flex-col px-2.5 pt-2">
      <Skeleton className="mb-3 h-2.5 w-10" />
      {SKELETON_WIDTHS.map((width, index) => (
        <div key={index} className="flex h-9 items-center">
          <Skeleton className={cn("h-3", width)} />
        </div>
      ))}
    </div>
  );
}
