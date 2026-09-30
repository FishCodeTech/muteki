"use client";

import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AnimatePresence } from "motion/react";
import { cn } from "@/lib/cn";
import { copyToClipboard } from "@/lib/clipboard";
import { Icon } from "@/components/Icon";
import {
  Collapse,
  EmptyState,
  IconButton,
  Menu,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  Popover,
  ScrollArea,
  SearchInput,
  Shortcut,
  Tooltip,
  handleMenuKeyDown,
  splitShortcut,
  toast,
  useReducedMotion,
  type Placement,
} from "@/components/chat/ui";
import {
  FolderHeader,
  ROW_SELECTOR,
  SearchHits,
  SectionLabel,
  SkeletonRows,
  ThreadRow,
  type RowDragHandlers,
} from "./rows";
import type { NavItem, NavSection, SidebarNavProps, SidebarSortMode } from "./types";

const COLLAPSED_SECTIONS_KEY = "muteki.sidebar.collapsed-sections.v1";

type MenuTarget =
  | { kind: "item"; item: NavItem; section: NavSection }
  | { kind: "folder"; section: NavSection; sectionKey: string }
  | { kind: "pinned"; section: NavSection };

interface OpenMenu {
  key: string;
  target: MenuTarget;
  point: { x: number; y: number } | null;
  placement: Placement;
  returnFocus: HTMLElement | null;
}

type PointerDragState = {
  kind: "item" | "section";
  sourceId: string;
  sectionKey: string;
  label: string;
  startIndex: number;
  startX: number;
  startY: number;
  active: boolean;
  reorder: (sourceId: string, targetId: string) => void;
};

const SORT_LABEL: Record<SidebarSortMode, string> = {
  priority: "优先级",
  updated: "最近更新",
  manual: "手动排序",
};

const SORT_DESCRIPTION: Record<SidebarSortMode, string> = {
  priority: "运行中与待处理的对话靠前",
  updated: "按最后活动时间分组",
  manual: "拖拽或 ⌥↑ / ⌥↓ 调整顺序",
};

function readCollapsed(): Set<string> {
  try {
    const saved = JSON.parse(localStorage.getItem(COLLAPSED_SECTIONS_KEY) || "[]");
    return new Set(Array.isArray(saved) ? saved.filter((id): id is string => typeof id === "string") : []);
  } catch {
    return new Set();
  }
}

function persistCollapsed(ids: Set<string>): void {
  try {
    localStorage.setItem(COLLAPSED_SECTIONS_KEY, JSON.stringify([...ids]));
  } catch {
    // Collapse state is cosmetic; ignore storage failures.
  }
}

/** Body search needs 1 CJK char or 2 other chars before it hits the server. */
export function queryLooksSearchable(raw: string): boolean {
  const q = raw.trim();
  if (!q) return false;
  return /[\u4e00-\u9fff]/.test(q) ? q.length >= 1 : q.length >= 2;
}

function sectionKeyOf(section: NavSection): string {
  return section.id || section.title;
}

function absoluteHref(href: string): string {
  try {
    return new URL(href, window.location.origin).toString();
  } catch {
    return href;
  }
}

export function SidebarNav({
  sections,
  activitySections = [],
  activityView = false,
  activityBadge = 0,
  onToggleActivityView,
  activeId,
  onSelect,
  onNewChat,
  onCreateFolder,
  searchQuery = "",
  onSearchChange,
  sortMode = "updated",
  onSortChange,
  groupMode = "project",
  onGroupChange,
  className = "",
  bodyHits = [],
  bodyHitsLoading = false,
  bodyHitsLoadingMore = false,
  bodyHitsError = "",
  bodyHitsHasMore = false,
  onLoadMoreBodyHits,
  onRetryBodyHits,
  includeSuperseded = false,
  onIncludeSupersededChange,
  onSelectBodyHit,
  loading = false,
}: SidebarNavProps) {
  const reduced = useReducedMotion();
  const [nowMs, setNowMs] = useState(() => Date.now());
  const [collapsedIds, setCollapsedIds] = useState<Set<string>>(() => new Set());
  const [projectsCollapsed, setProjectsCollapsed] = useState(false);
  const [expandedPreviewIds, setExpandedPreviewIds] = useState<Set<string>>(() => new Set());
  const [menu, setMenu] = useState<OpenMenu | null>(null);
  const [draggedItem, setDraggedItem] = useState<{ sectionKey: string; itemId: string } | null>(null);
  const [dragOverItem, setDragOverItem] = useState<{ sectionKey: string; itemId: string } | null>(null);
  const [draggedSection, setDraggedSection] = useState<string | null>(null);
  const [dragOverSection, setDragOverSection] = useState<string | null>(null);
  const [liveMessage, setLiveMessage] = useState("");

  const listRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const menuAnchorRef = useRef<HTMLElement | null>(null);
  const lastMenuRef = useRef<OpenMenu | null>(null);
  const didDragRef = useRef(false);
  const pointerDragRef = useRef<PointerDragState | null>(null);

  const query = searchQuery.trim();
  const searching = query.length > 0;

  useEffect(() => {
    setCollapsedIds(readCollapsed());
  }, []);

  useEffect(() => {
    const timer = window.setInterval(() => setNowMs(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    if (!onToggleActivityView) return;
    const onShortcut = (event: KeyboardEvent) => {
      if (event.altKey && (event.metaKey || event.ctrlKey) && event.code === "KeyU") {
        event.preventDefault();
        onToggleActivityView();
      }
    };
    window.addEventListener("keydown", onShortcut);
    return () => window.removeEventListener("keydown", onShortcut);
  }, [onToggleActivityView]);

  const toggleSection = useCallback((key: string) => {
    setCollapsedIds((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      persistCollapsed(next);
      return next;
    });
  }, []);

  const setSectionCollapsed = useCallback((key: string, value: boolean) => {
    setCollapsedIds((current) => {
      if (current.has(key) === value) return current;
      const next = new Set(current);
      if (value) next.add(key);
      else next.delete(key);
      persistCollapsed(next);
      return next;
    });
  }, []);

  /* ── Menus ─────────────────────────────────────────────────────────────── */

  useEffect(() => {
    if (menu) lastMenuRef.current = menu;
  }, [menu]);

  const closeMenu = useCallback(() => {
    setMenu((current) => {
      const returnFocus = current?.returnFocus;
      if (returnFocus) {
        window.requestAnimationFrame(() => {
          const active = document.activeElement as HTMLElement | null;
          if (!active || active === document.body || active.closest("[data-cx-layer]")) {
            if (returnFocus.isConnected) returnFocus.focus({ preventScroll: true });
          }
        });
      }
      return null;
    });
  }, []);

  const openMenuFromButton = useCallback((key: string, target: MenuTarget, anchor: HTMLElement, returnFocus: HTMLElement | null) => {
    if (menu?.key === key && !menu.point) {
      closeMenu();
      return;
    }
    menuAnchorRef.current = anchor;
    setMenu({ key, target, point: null, placement: "bottom-end", returnFocus });
  }, [closeMenu, menu]);

  const openMenuAtPoint = useCallback((
    event: React.MouseEvent<HTMLElement>,
    key: string,
    target: MenuTarget,
    returnFocus: HTMLElement | null,
  ) => {
    event.preventDefault();
    event.stopPropagation();
    menuAnchorRef.current = null;
    setMenu({ key, target, point: { x: event.clientX, y: event.clientY }, placement: "bottom-start", returnFocus });
  }, []);

  const rowFocusTarget = (wrapper: Element | null): HTMLElement | null => (
    wrapper?.querySelector<HTMLElement>(ROW_SELECTOR) ?? null
  );

  /* ── Pointer drag reordering ───────────────────────────────────────────── */

  const clearDragState = useCallback(() => {
    pointerDragRef.current = null;
    setDraggedItem(null);
    setDragOverItem(null);
    setDraggedSection(null);
    setDragOverSection(null);
    window.setTimeout(() => {
      didDragRef.current = false;
    }, 0);
  }, []);

  const dropTargetAt = (x: number, y: number, kind: PointerDragState["kind"]) => (
    document.elementFromPoint(x, y)?.closest<HTMLElement>(
      kind === "item" ? "[data-sidebar-item]" : "[data-cx-folder-header][data-sidebar-section]",
    ) ?? null
  );

  const dragHandlers = (drag: Omit<PointerDragState, "startX" | "startY" | "active">): RowDragHandlers => ({
    onPointerDown: (event) => {
      if (event.button !== 0 || event.pointerType !== "mouse") return;
      event.currentTarget.setPointerCapture(event.pointerId);
      pointerDragRef.current = { ...drag, startX: event.clientX, startY: event.clientY, active: false };
    },
    onPointerMove: (event) => {
      const current = pointerDragRef.current;
      if (!current) return;
      if (!current.active) {
        if (Math.hypot(event.clientX - current.startX, event.clientY - current.startY) < 5) return;
        current.active = true;
        didDragRef.current = true;
        closeMenu();
        if (current.kind === "item") {
          setDraggedItem({ sectionKey: current.sectionKey, itemId: current.sourceId });
          setLiveMessage(`已拿起对话 ${current.label}，当前位置第 ${current.startIndex + 1} 项`);
        } else {
          setDraggedSection(current.sourceId);
          setLiveMessage(`已拿起项目 ${current.label}`);
        }
      }
      event.preventDefault();
      const target = dropTargetAt(event.clientX, event.clientY, current.kind);
      if (current.kind === "item") {
        if (target?.dataset.sidebarSection === current.sectionKey && target.dataset.sidebarItem) {
          setDragOverItem({ sectionKey: current.sectionKey, itemId: target.dataset.sidebarItem });
        }
      } else if (target?.dataset.sidebarSection) {
        setDragOverSection(target.dataset.sidebarSection);
      }
    },
    onPointerUp: (event) => {
      const current = pointerDragRef.current;
      if (!current) return;
      if (!current.active) {
        pointerDragRef.current = null;
        return;
      }
      event.preventDefault();
      const target = dropTargetAt(event.clientX, event.clientY, current.kind);
      if (current.kind === "item") {
        const targetId = target?.dataset.sidebarItem;
        if (target?.dataset.sidebarSection === current.sectionKey && targetId && targetId !== current.sourceId) {
          current.reorder(current.sourceId, targetId);
          setLiveMessage(`对话 ${current.label} 已移动到第 ${Number(target.dataset.sidebarIndex || "0") + 1} 项`);
        }
      } else {
        const targetId = target?.dataset.sidebarSection;
        if (targetId && targetId !== current.sourceId) {
          current.reorder(current.sourceId, targetId);
          setLiveMessage(`项目 ${current.label} 已移动到 ${target?.dataset.sidebarLabel || "目标位置"} 之前`);
        }
      }
      clearDragState();
    },
    onPointerCancel: clearDragState,
  });

  /* ── Keyboard navigation ───────────────────────────────────────────────── */

  const visibleRows = () => Array.from(listRef.current?.querySelectorAll<HTMLElement>(ROW_SELECTOR) ?? [])
    .filter((el) => el.getClientRects().length > 0 && !el.closest("[inert]"));

  const onListKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement;
    if (!target.matches(ROW_SELECTOR) || event.altKey || event.metaKey || event.ctrlKey) return;
    const rows = visibleRows();
    const index = rows.indexOf(target);
    if (index < 0) return;
    if (event.key === "ArrowDown") {
      event.preventDefault();
      rows[Math.min(rows.length - 1, index + 1)]?.focus();
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      if (index === 0) searchRef.current?.focus();
      else rows[index - 1]?.focus();
    } else if (event.key === "Home") {
      event.preventDefault();
      rows[0]?.focus();
    } else if (event.key === "End") {
      event.preventDefault();
      rows[rows.length - 1]?.focus();
    }
  };

  const refocusItem = (itemId: string) => {
    window.requestAnimationFrame(() => {
      const wrapper = listRef.current?.querySelector(`[data-sidebar-item="${CSS.escape(itemId)}"]`);
      rowFocusTarget(wrapper ?? null)?.focus({ preventScroll: false });
    });
  };

  const isMenuKey = (event: React.KeyboardEvent) => event.key === "ContextMenu" || (event.shiftKey && event.key === "F10");

  const onRowKeyDown = (event: React.KeyboardEvent<HTMLElement>, item: NavItem, section: NavSection, sectionKey: string) => {
    if (isMenuKey(event)) {
      event.preventDefault();
      const wrapper = event.currentTarget.closest("[data-sidebar-item]");
      const more = wrapper?.querySelector<HTMLElement>("[data-cx-row-more]");
      if (more) openMenuFromButton(`item:${sectionKey}:${item.id}`, { kind: "item", item, section }, more, event.currentTarget);
      return;
    }
    if (event.altKey && (event.key === "ArrowUp" || event.key === "ArrowDown")) {
      const move = event.key === "ArrowUp" ? item.onMoveUp : item.onMoveDown;
      if (!move) return;
      event.preventDefault();
      move();
      setLiveMessage(`对话 ${item.label} 已${event.key === "ArrowUp" ? "上移" : "下移"}`);
      refocusItem(item.id);
    }
  };

  const openFirstResult = () => {
    const first = visibleRows().find((el) => el.hasAttribute("data-cx-openable"));
    first?.click();
  };

  const flatItems = useMemo(() => sections.flatMap((section) => section.items), [sections]);

  const onSearchKeyDown = (event: React.KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      visibleRows()[0]?.focus();
      return;
    }
    if (event.key === "Enter" && searching) {
      event.preventDefault();
      openFirstResult();
      return;
    }
    if ((event.metaKey || event.ctrlKey) && /^[1-9]$/.test(event.key)) {
      const index = Number(event.key) - 1;
      if (bodyHits.length > index) {
        event.preventDefault();
        const hit = bodyHits[index];
        onSelectBodyHit?.({ threadId: hit.threadId, messageId: hit.messageId });
        onSearchChange?.("");
        return;
      }
      const item = flatItems[index];
      if (!item) return;
      event.preventDefault();
      onSelect(item.id);
      onSearchChange?.("");
    }
  };

  /* ── Menu content ──────────────────────────────────────────────────────── */

  const renderMenu = (open: OpenMenu, close: () => void) => {
    const run = (fn?: () => void) => () => {
      close();
      fn?.();
    };
    const { target } = open;
    switch (target.kind) {
      case "item": {
        const { item, section } = target;
        const manual = (section.kind === "pinned" ? section.sortMode : sortMode) === "manual";
        const href = item.href;
        return (
          <>
            {item.onRename ? <MenuItem icon="pencil" onSelect={run(item.onRename)}>重命名</MenuItem> : null}
            {item.onPin ? (
              <MenuItem
                icon={item.pinned ? "pinOff" : "pin"}
                onSelect={run(() => {
                  setLiveMessage(item.pinned ? `已取消置顶 ${item.label}` : `已置顶 ${item.label}`);
                  item.onPin?.();
                })}
              >
                {item.pinned ? "取消置顶" : "置顶"}
              </MenuItem>
            ) : null}
            {item.onFork ? <MenuItem icon="gitFork" onSelect={run(item.onFork)}>分叉对话</MenuItem> : null}
            {href ? (
              <>
                <MenuItem
                  icon="link"
                  onSelect={run(() => {
                    void copyToClipboard(absoluteHref(href)).then((ok) => {
                      setLiveMessage(ok ? "已复制对话链接" : "复制失败");
                      toast({ title: ok ? "已复制对话链接" : "复制失败，请手动复制", tone: ok ? "success" : "danger" });
                    });
                  })}
                >
                  复制链接
                </MenuItem>
                <MenuItem icon="externalLink" onSelect={run(() => { window.open(href, "_blank", "noopener"); })}>
                  在新标签页打开
                </MenuItem>
              </>
            ) : null}
            {manual && (item.onMoveUp || item.onMoveDown) ? (
              <>
                <MenuSeparator />
                <MenuItem icon="arrowUp" shortcut="alt+up" disabled={!item.onMoveUp} onSelect={run(() => { item.onMoveUp?.(); setLiveMessage(`对话 ${item.label} 已上移`); })}>上移</MenuItem>
                <MenuItem icon="arrowDown" shortcut="alt+down" disabled={!item.onMoveDown} onSelect={run(() => { item.onMoveDown?.(); setLiveMessage(`对话 ${item.label} 已下移`); })}>下移</MenuItem>
              </>
            ) : null}
            {item.onArchive ? (
              <>
                <MenuSeparator />
                <MenuItem icon="archive" danger onSelect={run(item.onArchive)}>{item.archived ? "取消归档" : "归档"}</MenuItem>
              </>
            ) : null}
          </>
        );
      }
      case "folder": {
        const { section, sectionKey } = target;
        const collapsedNow = collapsedIds.has(sectionKey);
        return (
          <>
            {section.onNewChat ? <MenuItem icon="edit" onSelect={run(section.onNewChat)}>在此项目中新建对话</MenuItem> : null}
            <MenuItem icon={collapsedNow ? "unfoldVertical" : "foldVertical"} onSelect={run(() => setSectionCollapsed(sectionKey, !collapsedNow))}>
              {collapsedNow ? "展开项目" : "收起项目"}
            </MenuItem>
            {section.onMoveUp || section.onMoveDown ? (
              <>
                <MenuSeparator />
                <MenuItem icon="arrowUp" disabled={!section.onMoveUp} onSelect={run(() => { section.onMoveUp?.(); setLiveMessage(`项目 ${section.title} 已上移`); })}>上移项目</MenuItem>
                <MenuItem icon="arrowDown" disabled={!section.onMoveDown} onSelect={run(() => { section.onMoveDown?.(); setLiveMessage(`项目 ${section.title} 已下移`); })}>下移项目</MenuItem>
              </>
            ) : null}
            {section.onArchiveAll && section.items.length ? (
              <>
                <MenuSeparator />
                <MenuItem icon="archive" danger onSelect={run(section.onArchiveAll)} description={`共 ${section.items.length} 个对话`}>
                  归档项目内全部对话
                </MenuItem>
              </>
            ) : null}
          </>
        );
      }
      case "pinned": {
        const { section } = target;
        return (
          <>
            <MenuLabel>置顶排序方式</MenuLabel>
            {(["priority", "updated", "manual"] as const).map((mode) => (
              <MenuItem key={mode} checked={section.sortMode === mode} description={SORT_DESCRIPTION[mode]} onSelect={run(() => section.onSortChange?.(mode))}>
                {SORT_LABEL[mode]}
              </MenuItem>
            ))}
          </>
        );
      }
      default: {
        const exhaustive: never = target;
        return exhaustive;
      }
    }
  };

  /* ── Section rendering ─────────────────────────────────────────────────── */

  const visibleSections = activityView ? activitySections : sections;
  const pinnedSections = activityView ? [] : visibleSections.filter((section) => section.kind === "pinned");
  const folderSections = activityView ? [] : visibleSections.filter((section) => section.kind === "folder");
  const otherSections = activityView
    ? visibleSections
    : visibleSections.filter((section) => section.kind !== "pinned" && section.kind !== "folder");

  const renderRows = (section: NavSection, sectionKey: string, items: NavItem[], indent: boolean) => {
    const isActivity = section.kind === "activity";
    const reorderable = Boolean(section.onReorderItems) && !isActivity && !searching;
    return (
      <AnimatePresence initial={false}>
        {items.map((item, index) => (
          <ThreadRow
            key={item.id}
            item={item}
            sectionKey={sectionKey}
            index={index}
            active={item.id === activeId}
            variant={isActivity ? "activity" : "default"}
            indent={indent}
            nowMs={nowMs}
            highlight={query}
            menuOpen={menu?.key === `item:${sectionKey}:${item.id}`}
            dragging={draggedItem?.itemId === item.id && draggedItem.sectionKey === sectionKey}
            dragOver={dragOverItem?.itemId === item.id && dragOverItem.sectionKey === sectionKey && draggedItem?.itemId !== item.id}
            reduced={reduced}
            didDragRef={didDragRef}
            drag={reorderable && section.onReorderItems ? dragHandlers({
              kind: "item",
              sourceId: item.id,
              sectionKey,
              label: item.label,
              startIndex: index,
              reorder: section.onReorderItems,
            }) : undefined}
            onSelect={onSelect}
            onOpenMenu={(target, anchor) => openMenuFromButton(
              `item:${sectionKey}:${target.id}`,
              { kind: "item", item: target, section },
              anchor,
              rowFocusTarget(anchor.closest("[data-sidebar-item]")),
            )}
            onContextMenu={(event, target) => openMenuAtPoint(
              event,
              `item:${sectionKey}:${target.id}`,
              { kind: "item", item: target, section },
              rowFocusTarget(event.currentTarget),
            )}
            onKeyDown={(event, target) => onRowKeyDown(event, target, section, sectionKey)}
          />
        ))}
      </AnimatePresence>
    );
  };

  const previewItems = (section: NavSection, sectionKey: string) => {
    const limit = section.kind === "folder" && typeof section.previewLimit === "number" && section.previewLimit > 0
      ? Math.floor(section.previewLimit)
      : null;
    if (searching || limit === null || section.items.length <= limit) {
      return { items: section.items, overflow: 0, expanded: false, limit };
    }
    const expanded = expandedPreviewIds.has(sectionKey);
    if (expanded) return { items: section.items, overflow: section.items.length - limit, expanded, limit };
    let items = section.items.slice(0, limit);
    if (activeId && !items.some((item) => item.id === activeId)) {
      const activeItem = section.items.find((item) => item.id === activeId);
      if (activeItem) items = [...items.slice(0, limit - 1), activeItem];
    }
    return { items, overflow: section.items.length - items.length, expanded, limit };
  };

  const renderFolder = (section: NavSection, sectionIndex: number) => {
    const sectionKey = sectionKeyOf(section);
    const collapsed = !searching && collapsedIds.has(sectionKey);
    const bodyId = `sidebar-section-${sectionKey}`;
    const preview = previewItems(section, sectionKey);
    const menuKey = `folder:${sectionKey}`;
    return (
      <div key={sectionKey} className="flex flex-col">
        <FolderHeader
          section={section}
          sectionKey={sectionKey}
          collapsed={collapsed}
          count={section.items.length}
          menuOpen={menu?.key === menuKey}
          dragging={draggedSection === sectionKey}
          dragOver={dragOverSection === sectionKey && draggedSection !== sectionKey}
          controlsId={bodyId}
          didDragRef={didDragRef}
          drag={section.onReorderSection && !searching ? dragHandlers({
            kind: "section",
            sourceId: sectionKey,
            sectionKey,
            label: section.title,
            startIndex: sectionIndex,
            reorder: section.onReorderSection,
          }) : undefined}
          onToggle={() => toggleSection(sectionKey)}
          onOpenMenu={(anchor) => openMenuFromButton(
            menuKey,
            { kind: "folder", section, sectionKey },
            anchor,
            anchor.closest("[data-cx-folder-header]")?.querySelector<HTMLElement>(ROW_SELECTOR) ?? null,
          )}
          onContextMenu={(event) => openMenuAtPoint(
            event,
            menuKey,
            { kind: "folder", section, sectionKey },
            event.currentTarget.querySelector<HTMLElement>(ROW_SELECTOR),
          )}
          onKeyDown={(event) => {
            if (isMenuKey(event)) {
              event.preventDefault();
              const more = event.currentTarget.parentElement?.querySelector<HTMLElement>("[data-cx-row-more]");
              if (more) openMenuFromButton(menuKey, { kind: "folder", section, sectionKey }, more, event.currentTarget);
            } else if (event.key === "ArrowRight" && collapsed) {
              event.preventDefault();
              toggleSection(sectionKey);
            } else if (event.key === "ArrowLeft" && !collapsed) {
              event.preventDefault();
              toggleSection(sectionKey);
            }
          }}
        />
        <Collapse open={!collapsed}>
          <div id={bodyId} role="group" aria-label={section.title} className="flex flex-col gap-px pb-1">
            {!section.items.length && section.emptyLabel ? (
              <p className="flex h-8 items-center pl-[30px] text-[12.5px] text-cx-fg-4">{section.emptyLabel}</p>
            ) : null}
            {renderRows(section, sectionKey, preview.items, true)}
            {preview.overflow > 0 || preview.expanded ? (
              <button
                type="button"
                data-cx-nav-row=""
                aria-expanded={preview.expanded}
                onClick={() => {
                  const nextExpanded = !preview.expanded;
                  setExpandedPreviewIds((current) => {
                    const next = new Set(current);
                    if (nextExpanded) next.add(sectionKey);
                    else next.delete(sectionKey);
                    return next;
                  });
                  setLiveMessage(nextExpanded
                    ? `${section.title} 已展开全部 ${section.items.length} 个对话`
                    : `${section.title} 已收起，只显示 ${preview.limit} 个对话`);
                }}
                className="cx-press flex h-7 items-center gap-1 rounded-lg pl-[30px] text-left text-[12.5px] text-cx-fg-3 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
              >
                {preview.expanded ? "收起" : `显示更多（${preview.overflow}）`}
              </button>
            ) : null}
          </div>
        </Collapse>
      </div>
    );
  };

  const renderPlainSection = (section: NavSection) => {
    const sectionKey = sectionKeyOf(section);
    const isPinned = section.kind === "pinned";
    const isActivity = section.kind === "activity";
    if (!section.items.length && !(isActivity && section.emptyLabel)) return null;
    const collapsed = !searching && collapsedIds.has(sectionKey);
    const bodyId = `sidebar-section-${sectionKey}`;
    const menuKey = `pinned:${sectionKey}`;
    return (
      <section key={sectionKey} className="mt-2 flex flex-col first:mt-0" aria-label={section.title}>
        <SectionLabel
          title={section.title}
          collapsed={collapsed}
          controlsId={bodyId}
          onToggle={() => toggleSection(sectionKey)}
          actions={isPinned && section.onSortChange ? (
            <button
              type="button"
              aria-label="置顶排序方式"
              aria-haspopup="menu"
              aria-expanded={menu?.key === menuKey}
              onClick={(event) => openMenuFromButton(menuKey, { kind: "pinned", section }, event.currentTarget, event.currentTarget)}
              className="cx-press grid size-6 place-items-center rounded-md text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"
            >
              <Icon name="more" size={14} />
            </button>
          ) : undefined}
        />
        <Collapse open={!collapsed}>
          <div id={bodyId} role="group" aria-label={section.title} className="flex flex-col gap-px">
            {!section.items.length && section.emptyLabel ? (
              <p className="px-2.5 py-1.5 text-[12.5px] text-cx-fg-4">{section.emptyLabel}</p>
            ) : null}
            {renderRows(section, sectionKey, section.items, false)}
          </div>
        </Collapse>
      </section>
    );
  };

  const visibleFolders = searching ? folderSections.filter((section) => section.items.length) : folderSections;
  const showProjects = !activityView && groupMode === "project" && (visibleFolders.length > 0 || (!searching && Boolean(onCreateFolder)));
  const totalItems = visibleSections.reduce((total, section) => total + section.items.length, 0);
  const showSkeleton = loading && totalItems === 0 && !activityView && !searching;
  const showEmpty = !loading && totalItems === 0 && !activityView;

  const renderedMenu = menu ?? lastMenuRef.current;

  return (
    <nav className={cn("cx-sidebar-nav flex h-full min-h-0 flex-col", className)} aria-label="对话列表">
      <p className="sr-only" aria-live="polite" aria-atomic="true">{liveMessage}</p>

      <div className="flex flex-none flex-col gap-1.5 px-2 pb-2 pt-2">
        <button type="button" onClick={onNewChat} data-cx-new-chat="" className="cx-sb-new group/new">
          <Icon name="edit" size={16} />
          <span className="flex-1 text-left">新对话</span>
          <Shortcut keys="mod+shift+n" tone="subtle" className="cx-sb-new-kbd" />
        </button>
        {onSearchChange ? (
          <div className="flex items-center gap-1">
            <SearchInput
              ref={searchRef}
              value={searchQuery}
              onValueChange={onSearchChange}
              onKeyDown={onSearchKeyDown}
              placeholder="搜索对话"
              aria-label="搜索对话"
              aria-describedby="cx-sidebar-search-hint"
              className="min-w-0 flex-1"
            />
            <span id="cx-sidebar-search-hint" className="sr-only">回车打开第一个结果，方向键下移到列表</span>
            <Menu
              placement="bottom-end"
              ariaLabel="整理侧边栏"
              trigger={<IconButton icon="sliders" label="整理侧边栏" size="md" className="rounded-lg" />}
            >
              <MenuLabel>分组方式</MenuLabel>
              <MenuItem icon="folder" checked={groupMode === "project"} onSelect={() => onGroupChange?.("project")}>按项目</MenuItem>
              <MenuItem icon="rows" checked={groupMode === "list"} onSelect={() => onGroupChange?.("list")}>单一列表</MenuItem>
              <MenuSeparator />
              <MenuLabel>排序方式</MenuLabel>
              {(["updated", "priority", "manual"] as const).map((mode) => (
                <MenuItem key={mode} checked={sortMode === mode} description={SORT_DESCRIPTION[mode]} onSelect={() => onSortChange?.(mode)}>
                  {SORT_LABEL[mode]}
                </MenuItem>
              ))}
              {onCreateFolder ? (
                <>
                  <MenuSeparator />
                  <MenuItem icon="folderPlus" onSelect={onCreateFolder}>新建项目文件夹</MenuItem>
                </>
              ) : null}
            </Menu>
          </div>
        ) : null}
      </div>

      <ScrollArea
        ref={listRef}
        className="flex-1 overscroll-contain px-2 pb-3"
        onKeyDown={onListKeyDown}
      >
        {searching && !activityView && (bodyHits.length > 0 || bodyHitsLoading || queryLooksSearchable(query)) ? (
          <SearchHits
            hits={bodyHits}
            loading={bodyHitsLoading}
            loadingMore={bodyHitsLoadingMore}
            error={bodyHitsError}
            hasMore={bodyHitsHasMore}
            onLoadMore={onLoadMoreBodyHits}
            onRetry={onRetryBodyHits}
            activeId={activeId}
            includeSuperseded={includeSuperseded}
            onIncludeSupersededChange={onIncludeSupersededChange}
            onSelect={(hit) => {
              onSelectBodyHit?.({ threadId: hit.threadId, messageId: hit.messageId });
              onSearchChange?.("");
            }}
          />
        ) : null}

        {activityView ? (
          <div className="flex h-8 items-center justify-between pl-2.5 pr-1">
            <span className="text-[12.5px] font-medium text-cx-fg-2">收件箱</span>
            {onToggleActivityView ? (
              <button
                type="button"
                onClick={onToggleActivityView}
                className="cx-press flex h-6 items-center gap-1 rounded-md px-1.5 text-[11.5px] text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg"
              >
                <Icon name="arrowLeft" size={12} />
                返回列表
              </button>
            ) : null}
          </div>
        ) : null}

        {showSkeleton ? <SkeletonRows /> : null}

        {pinnedSections.map(renderPlainSection)}

        {showProjects ? (
          <section className="mt-2 flex flex-col first:mt-0" aria-label="项目">
            <SectionLabel
              title="项目"
              collapsed={!searching && projectsCollapsed}
              controlsId="sidebar-projects"
              onToggle={() => setProjectsCollapsed((value) => !value)}
              actions={onCreateFolder ? (
                <button
                  type="button"
                  aria-label="新建项目文件夹"
                  title="新建项目文件夹"
                  onClick={onCreateFolder}
                  className="cx-press grid size-6 place-items-center rounded-md text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"
                >
                  <Icon name="folderPlus" size={14} />
                </button>
              ) : undefined}
            />
            <Collapse open={searching || !projectsCollapsed}>
              <div id="sidebar-projects" className="flex flex-col gap-px">
                {visibleFolders.map(renderFolder)}
                {!visibleFolders.length && onCreateFolder ? (
                  <button
                    type="button"
                    data-cx-nav-row=""
                    onClick={onCreateFolder}
                    className="cx-press flex h-8 items-center gap-2 rounded-lg pl-2.5 text-left text-[13.5px] text-cx-fg-3 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
                  >
                    <Icon name="folderPlus" size={15} />
                    新建项目文件夹
                  </button>
                ) : null}
              </div>
            </Collapse>
          </section>
        ) : null}

        {otherSections.map(renderPlainSection)}

        {showEmpty ? (
          searching ? (
            <EmptyState compact icon="search" title="没有匹配的对话" description={`找不到标题包含「${query}」的对话`} />
          ) : (
            <EmptyState
              compact
              icon="messageCircle"
              title="还没有对话"
              description="开始一段新对话，它会出现在这里。"
            />
          )
        ) : null}
      </ScrollArea>

      <div className="flex flex-none items-center gap-1 border-t border-cx-border-subtle px-2 py-2">
        {onToggleActivityView ? (
          <Tooltip content={activityView ? "返回对话列表" : "需要关注的对话"} shortcut={splitShortcut("alt+mod+u")}>
            <button
              type="button"
              aria-pressed={activityView}
              data-active={activityView ? "true" : undefined}
              aria-label={activityBadge > 0 ? `收件箱，${activityBadge} 项待处理` : "收件箱"}
              onClick={onToggleActivityView}
              className={cn(
                "cx-press flex h-8 min-w-0 flex-1 items-center gap-2.5 rounded-lg px-2.5 text-[13px] text-cx-fg-2 outline-none",
                "hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
                activityView && "bg-cx-active font-medium text-cx-fg hover:bg-cx-active",
              )}
            >
              <Icon name="inbox" size={16} className="shrink-0" />
              <span className="flex-1 truncate text-left">收件箱</span>
              {activityBadge > 0 ? (
                <span className="cx-tabular inline-flex h-[18px] min-w-[18px] items-center justify-center rounded-full bg-cx-accent px-1.5 text-[11px] font-semibold leading-none text-cx-accent-fg">
                  {activityBadge > 99 ? "99+" : activityBadge}
                </span>
              ) : null}
            </button>
          </Tooltip>
        ) : <span className="flex-1" />}
        <Tooltip content="设置">
          <a
            href="/settings/agents"
            aria-label="打开设置"
            className="cx-press grid size-8 shrink-0 place-items-center rounded-lg text-cx-fg-3 no-underline outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
          >
            <Icon name="gear" size={16} />
          </a>
        </Tooltip>
      </div>

      <Popover
        open={Boolean(menu)}
        onOpenChange={(open) => { if (!open) closeMenu(); }}
        anchorRef={menuAnchorRef}
        anchorPoint={menu?.point ?? null}
        placement={menu?.placement ?? "bottom-end"}
        role="menu"
        haspopup="menu"
        ariaLabel="对话操作"
        className="min-w-[212px] p-1"
        onKeyDown={handleMenuKeyDown}
        restoreFocus={false}
      >
        {({ close }) => (renderedMenu ? (
          <div className="cx-scroll flex min-h-0 flex-col overflow-y-auto">{renderMenu(renderedMenu, close)}</div>
        ) : null)}
      </Popover>
    </nav>
  );
}
