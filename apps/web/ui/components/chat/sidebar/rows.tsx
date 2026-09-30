"use client";

import type { KeyboardEvent, MouseEvent, PointerEvent, ReactNode, RefObject } from "react";
import { motion } from "motion/react";
import { useLang } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Skeleton, Spinner, StatusDot, SPRING_LAYOUT, FADE_FAST } from "@/components/chat/ui";
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

function StatusIndicator({ item }: { item: NavItem }) {
  if (item.running) {
    return (
      <span className="grid size-4 place-items-center text-cx-accent" title="正在运行">
        <Spinner size={12} />
        <span className="sr-only">正在运行</span>
      </span>
    );
  }
  if (item.failed) {
    return <span className="grid size-4 place-items-center" title="会话异常"><StatusDot tone="danger" label="会话异常" className="size-[7px]" /></span>;
  }
  if (item.needsAction) {
    return <span className="grid size-4 place-items-center" title="待审批或等待输入"><StatusDot tone="warning" label="待审批或等待输入" className="size-[7px]" /></span>;
  }
  if (item.unread) {
    return <span className="grid size-4 place-items-center" title="有未读更新"><StatusDot tone="accent" label="有未读更新" className="size-[7px]" /></span>;
  }
  return null;
}

function hasIndicator(item: NavItem): boolean {
  return Boolean(item.running || item.failed || item.needsAction || item.unread);
}

export interface ThreadRowProps {
  item: NavItem;
  sectionKey: string;
  index: number;
  active: boolean;
  variant?: "default" | "activity";
  indent?: boolean;
  nowMs: number;
  highlight?: string;
  menuOpen: boolean;
  dragging: boolean;
  dragOver: boolean;
  reduced: boolean;
  didDragRef: RefObject<boolean>;
  drag?: RowDragHandlers;
  onSelect: (id: string) => void;
  onOpenMenu: (item: NavItem, anchor: HTMLElement) => void;
  onContextMenu: (event: MouseEvent<HTMLElement>, item: NavItem) => void;
  onKeyDown: (event: KeyboardEvent<HTMLElement>, item: NavItem) => void;
}

export function ThreadRow({
  item,
  sectionKey,
  index,
  active,
  variant = "default",
  indent = false,
  nowMs,
  highlight = "",
  menuOpen,
  dragging,
  dragOver,
  reduced,
  didDragRef,
  drag,
  onSelect,
  onOpenMenu,
  onContextMenu,
  onKeyDown,
}: ThreadRowProps) {
  const time = formatCompactRelative(item.updatedAt, nowMs);
  const indicator = hasIndicator(item);
  const activity = variant === "activity";
  const exactTime = item.updatedAt && Number.isFinite(Date.parse(item.updatedAt))
    ? new Date(item.updatedAt).toLocaleString()
    : undefined;

  return (
    <motion.div
      layout={reduced ? false : "position"}
      initial={reduced ? false : { opacity: 0, y: -4 }}
      animate={{ opacity: 1, y: 0 }}
      exit={reduced ? { opacity: 0 } : { opacity: 0, transition: FADE_FAST }}
      transition={SPRING_LAYOUT}
      className={cn(
        "group/row relative",
        dragging && "opacity-45",
        dragOver && "before:pointer-events-none before:absolute before:inset-x-2 before:-top-px before:z-10 before:h-0.5 before:rounded-full before:bg-cx-accent",
      )}
      data-sidebar-section={sectionKey}
      data-sidebar-item={item.id}
      data-sidebar-index={index}
      data-selected={active ? "true" : undefined}
      data-menu-open={menuOpen ? "true" : undefined}
      onContextMenu={(event) => onContextMenu(event, item)}
    >
      <a
        href={item.href || "#"}
        draggable={false}
        data-cx-nav-row=""
        data-cx-openable=""
        aria-current={active ? "page" : undefined}
        aria-keyshortcuts="Shift+F10"
        title={item.label}
        onClick={(event) => {
          if (didDragRef.current) {
            event.preventDefault();
            return;
          }
          if (item.href && (event.metaKey || event.ctrlKey || event.shiftKey || event.button !== 0)) return;
          event.preventDefault();
          onSelect(item.id);
        }}
        onKeyDown={(event) => onKeyDown(event, item)}
        onPointerDown={drag?.onPointerDown}
        onPointerMove={drag?.onPointerMove}
        onPointerUp={drag?.onPointerUp}
        onPointerCancel={drag?.onPointerCancel}
        style={{ paddingRight: indicator ? 58 : 40 }}
        className={cn(
          "flex w-full min-w-0 select-none items-center rounded-lg text-[13.5px] leading-5 no-underline outline-none",
          "cx-press text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg",
          "focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
          "group-data-[menu-open=true]/row:bg-cx-hover",
          activity ? "min-h-11 py-1.5" : "h-8",
          indent ? "pl-[30px]" : "pl-2.5",
          active && "bg-cx-active font-medium text-cx-fg hover:bg-cx-active group-data-[menu-open=true]/row:bg-cx-active",
        )}
      >
        {activity ? (
          <span className="flex min-w-0 flex-1 flex-col">
            <span className="cx-sb-fade min-w-0 overflow-hidden whitespace-nowrap">{highlightMatch(item.label, highlight)}</span>
            {item.subtitle ? (
              <span className="flex min-w-0 items-center gap-1 text-[12px] font-normal leading-4 text-cx-fg-4">
                <Icon name="folder" size={12} className="shrink-0" />
                <span className="truncate">{item.subtitle}</span>
              </span>
            ) : null}
          </span>
        ) : (
          <span className="cx-sb-fade min-w-0 flex-1 overflow-hidden whitespace-nowrap">{highlightMatch(item.label, highlight)}</span>
        )}
      </a>
      <div className="pointer-events-none absolute inset-y-0 right-1 flex items-center gap-1">
        {indicator ? <StatusIndicator item={item} /> : null}
        <span className="relative grid h-6 min-w-7 place-items-center">
          {time ? (
            <span
              className={cn(
                "cx-tabular text-[11.5px] leading-none text-cx-fg-4 transition-opacity duration-100",
                "group-hover/row:opacity-0 group-has-[:focus-visible]/row:opacity-0 group-data-[menu-open=true]/row:opacity-0",
              )}
              title={exactTime}
            >
              {time}
            </span>
          ) : null}
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
              onOpenMenu(item, event.currentTarget);
            }}
            className={cn(
              "pointer-events-auto absolute inset-0 grid place-items-center rounded-md text-cx-fg-3 opacity-0 outline-none",
              "cx-press hover:bg-cx-active hover:text-cx-fg",
              "group-hover/row:opacity-100 group-has-[:focus-visible]/row:opacity-100 group-data-[menu-open=true]/row:opacity-100 group-data-[menu-open=true]/row:text-cx-fg",
            )}
          >
            <Icon name="more" size={15} />
          </button>
        </span>
      </div>
    </motion.div>
  );
}

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
    <div className={cn("group/label flex h-8 items-center justify-between gap-1 pl-2.5 pr-1", className)}>
      <button
        type="button"
        data-cx-nav-row=""
        aria-expanded={!collapsed}
        aria-controls={controlsId}
        onClick={onToggle}
        className="-ml-1 flex h-6 min-w-0 items-center gap-1 rounded-md px-1 text-[11.5px] font-medium text-cx-fg-4 outline-none transition-colors hover:text-cx-fg-2 focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
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

export function FolderHeader({
  section,
  sectionKey,
  collapsed,
  count,
  menuOpen,
  dragging,
  dragOver,
  controlsId,
  didDragRef,
  drag,
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
  dragging: boolean;
  dragOver: boolean;
  controlsId: string;
  didDragRef: RefObject<boolean>;
  drag?: RowDragHandlers;
  onToggle: () => void;
  onOpenMenu: (anchor: HTMLElement) => void;
  onContextMenu: (event: MouseEvent<HTMLElement>) => void;
  onKeyDown: (event: KeyboardEvent<HTMLElement>) => void;
}) {
  return (
    <div
      className={cn(
        "group/folder relative",
        dragging && "opacity-45",
        dragOver && "before:pointer-events-none before:absolute before:inset-x-2 before:-top-px before:z-10 before:h-0.5 before:rounded-full before:bg-cx-accent",
      )}
      data-cx-folder-header=""
      data-sidebar-section={sectionKey}
      data-sidebar-label={section.title}
      data-menu-open={menuOpen ? "true" : undefined}
      onContextMenu={onContextMenu}
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
          onToggle();
        }}
        onKeyDown={onKeyDown}
        onPointerDown={drag?.onPointerDown}
        onPointerMove={drag?.onPointerMove}
        onPointerUp={drag?.onPointerUp}
        onPointerCancel={drag?.onPointerCancel}
        className={cn(
          "flex min-h-8 w-full min-w-0 select-none items-center gap-2 rounded-lg pl-2.5 pr-[62px] text-left text-[13.5px] leading-5 text-cx-fg-2 outline-none",
          "cx-press hover:bg-cx-hover hover:text-cx-fg group-data-[menu-open=true]/folder:bg-cx-hover",
          "focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
        )}
      >
        <Icon name={collapsed ? "folder" : "folderOpen"} size={15} className="shrink-0 text-cx-fg-3" />
        <span className="min-w-0 flex-1 py-1">
          <span className="block truncate">{section.title}</span>
          {section.subtitle ? <span className="block truncate text-[10.5px] leading-4 text-cx-fg-4">{section.subtitle}</span> : null}
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
          <span className="cx-tabular min-w-6 pr-1.5 text-right text-[11.5px] text-cx-fg-4 transition-opacity duration-100 group-hover/folder:opacity-0 group-has-[:focus-visible]/folder:opacity-0 group-data-[menu-open=true]/folder:opacity-0">
            {count || ""}
          </span>
          <span className="pointer-events-auto absolute right-0 flex items-center gap-0.5 opacity-0 transition-opacity duration-100 group-hover/folder:opacity-100 group-has-[:focus-visible]/folder:opacity-100 group-data-[menu-open=true]/folder:opacity-100">
            {section.onNewChat ? (
              <button
                type="button"
                tabIndex={-1}
                aria-label={`在 ${section.title} 中新建对话`}
                title="在此项目中新建对话"
                onClick={(event) => {
                  event.stopPropagation();
                  section.onNewChat?.();
                }}
                className="cx-press grid size-6 place-items-center rounded-md text-cx-fg-3 hover:bg-cx-active hover:text-cx-fg"
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
                onOpenMenu(event.currentTarget);
              }}
              className="cx-press grid size-6 place-items-center rounded-md text-cx-fg-3 hover:bg-cx-active hover:text-cx-fg"
            >
              <Icon name="more" size={15} />
            </button>
          </span>
        </span>
      </div>
    </div>
  );
}

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
        <span className="text-[11.5px] font-medium text-cx-fg-4">消息</span>
        {onIncludeSupersededChange ? (
          <button
            type="button"
            aria-pressed={includeSuperseded}
            onClick={() => onIncludeSupersededChange(!includeSuperseded)}
            className={cn(
              "cx-press flex h-6 items-center gap-1 rounded-md px-1.5 text-[11.5px] outline-none",
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
          {hits.map((hit, index) => (
            <button
              key={`${hit.threadId}:${hit.messageId}`}
              type="button"
              data-cx-nav-row=""
              data-cx-openable=""
              aria-current={hit.threadId === activeId ? "true" : undefined}
              title={index < 9 ? `⌘${index + 1} 打开` : undefined}
              onClick={() => onSelect(hit)}
              className={cn(
                "cx-press flex w-full min-w-0 flex-col gap-0.5 rounded-lg px-2.5 py-1.5 text-left outline-none",
                "hover:bg-cx-hover focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
                hit.threadId === activeId && "bg-cx-active",
              )}
            >
              <span className="flex min-w-0 items-center gap-1.5">
                <span className="min-w-0 flex-1 truncate text-[13px] font-medium leading-5 text-cx-fg">{hit.title || "未命名对话"}</span>
                {hit.archived ? <span className="shrink-0 rounded px-1 text-[10.5px] leading-4 text-cx-fg-4 ring-1 ring-cx-border ring-inset">已归档</span> : null}
                {hit.superseded ? <span className="shrink-0 rounded px-1 text-[10.5px] leading-4 text-cx-fg-4 ring-1 ring-cx-border ring-inset">已替代</span> : null}
                <span className="shrink-0 text-[11px] text-cx-fg-4">{roleLabel(hit.role)}</span>
              </span>
              <span className="line-clamp-2 text-[12px] leading-[18px] text-cx-fg-3">{renderSnippet(String(hit.snippet || ""))}</span>
            </button>
          ))}
        </div>
      ) : !error ? (
        <p className="px-2.5 py-1.5 text-[12.5px] text-cx-fg-4">{english ? "No matching message text" : "没有匹配的消息正文"}</p>
      ) : null}
      {error ? <div role="alert" className="mx-2.5 my-2 rounded-md border border-cx-border p-2 text-[12px] text-cx-warning">
        <p>{english ? "Message search did not finish." : "正文搜索未完成。"}</p>
        <details><summary>{english ? "Error details" : "错误详情"}</summary><pre className="mt-1 whitespace-pre-wrap break-all font-cx-mono text-[11px]">{error}</pre></details>
        {onRetry ? <button type="button" onClick={onRetry} disabled={loading || loadingMore} className="mt-1 rounded px-2 py-1 text-cx-accent focus-visible:outline-2">{english ? "Retry" : "重试"}</button> : null}
      </div> : null}
      {!loading && hits.length ? <p role="status" className="px-2.5 pt-1 text-[11px] text-cx-fg-4">{english ? `${hits.length} loaded${hasMore ? "; more results available" : "; all results loaded"}` : `已加载 ${hits.length} 条${hasMore ? "，还有更多结果" : "，已加载全部结果"}`}</p> : null}
      {!error && hasMore && onLoadMore ? <button type="button" onClick={onLoadMore} disabled={loadingMore} className="mx-2.5 mt-1 rounded-md px-2 py-1 text-[12px] text-cx-accent hover:bg-cx-hover focus-visible:outline-2">{loadingMore ? (english ? "Loading…" : "加载中…") : (english ? "Load more results" : "加载更多结果")}</button> : null}
    </section>
  );
}

const SKELETON_WIDTHS = ["w-[72%]", "w-[54%]", "w-[80%]", "w-[62%]", "w-[46%]", "w-[68%]", "w-[58%]"];

export function SkeletonRows() {
  return (
    <div role="status" aria-label="正在加载对话" className="flex flex-col px-2.5 pt-2">
      <Skeleton className="mb-3 h-2.5 w-10" />
      {SKELETON_WIDTHS.map((width, index) => (
        <div key={index} className="flex h-8 items-center">
          <Skeleton className={cn("h-3", width)} />
        </div>
      ))}
    </div>
  );
}
