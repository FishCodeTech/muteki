"use client";

import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
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
  MenuScope,
  MenuSeparator,
  MenuSub,
  Popover,
  ScrollArea,
  SearchInput,
  Shortcut,
  handleMenuKeyDown,
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
import { ThreadHoverCard } from "./ThreadHoverCard";
import { SnoozeDialog } from "./SnoozeDialog";
import { createDragGhost, type DragGhost } from "./dragGhost";
import { formatWakeTime, snoozePresets } from "@/lib/sidebarInbox";

const COLLAPSED_SECTIONS_KEY = "muteki.sidebar.collapsed-sections.v1";
const HOVER_CARD_DELAY_MS = 450;
const JUMP_HINT_DELAY_MS = 250;
const DRAG_SCROLL_EDGE = 40;
const DRAG_SCROLL_MAX_SPEED = 14;
const isMacPlatform = () => typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

type MenuTarget =
  | { kind: "item"; item: NavItem; section: NavSection }
  | { kind: "snooze"; item: NavItem }
  | { kind: "bulk"; ids: string[] }
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
  projectFilter,
  bulkActions,
  sections,
  activitySections = [],
  activityView = false,
  activityBadge = 0,
  onToggleActivityView,
  activityOptions,
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
  const [selectedIds, setSelectedIds] = useState<Set<string>>(() => new Set());
  const selectionAnchorRef = useRef<string | null>(null);
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [hoverCard, setHoverCard] = useState<{ item: NavItem; element: HTMLElement } | null>(null);
  const hoverTimerRef = useRef<number | undefined>(undefined);
  const [jumpHints, setJumpHints] = useState<Map<string, number> | null>(null);
  const [snoozeDialog, setSnoozeDialog] = useState<{ ids: string[]; subject: string; apply: (until: number) => void } | null>(null);
  const [pageLimits, setPageLimits] = useState<Record<string, number>>({});

  const listRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const menuAnchorRef = useRef<HTMLElement | null>(null);
  const lastMenuRef = useRef<OpenMenu | null>(null);
  const didDragRef = useRef(false);
  const pointerDragRef = useRef<PointerDragState | null>(null);
  const dragGhostRef = useRef<DragGhost | null>(null);
  const autoScrollRef = useRef({ frame: 0, speed: 0, x: 0, y: 0 });

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

  const markAllRead = activityOptions?.unreadCount ? activityOptions.onMarkAllRead : undefined;
  useEffect(() => {
    if (!markAllRead) return;
    const onShortcut = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || !event.shiftKey || event.altKey || event.metaKey || event.ctrlKey) return;
      if (event.defaultPrevented) return;
      event.preventDefault();
      markAllRead();
    };
    window.addEventListener("keydown", onShortcut);
    return () => window.removeEventListener("keydown", onShortcut);
  }, [markAllRead]);

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

  const stopAutoScroll = useCallback(() => {
    const auto = autoScrollRef.current;
    if (auto.frame) window.cancelAnimationFrame(auto.frame);
    auto.frame = 0;
    auto.speed = 0;
  }, []);

  const clearDragState = useCallback(() => {
    pointerDragRef.current = null;
    dragGhostRef.current?.cancel();
    dragGhostRef.current = null;
    stopAutoScroll();
    delete document.documentElement.dataset.cxSidebarDragging;
    setDraggedItem(null);
    setDragOverItem(null);
    setDraggedSection(null);
    setDragOverSection(null);
    window.setTimeout(() => {
      didDragRef.current = false;
    }, 0);
  }, [stopAutoScroll]);

  useEffect(() => clearDragState, [clearDragState]);

  const dropTargetAt = (x: number, y: number, kind: PointerDragState["kind"]) => (
    document.elementFromPoint(x, y)?.closest<HTMLElement>(
      kind === "item" ? "[data-sidebar-item]" : "[data-cx-folder-header][data-sidebar-section]",
    ) ?? null
  );

  const updateDropTarget = (current: PointerDragState, x: number, y: number) => {
    const target = dropTargetAt(x, y, current.kind);
    if (current.kind === "item") {
      const itemId = target?.dataset.sidebarItem;
      if (target?.dataset.sidebarSection === current.sectionKey && itemId) {
        setDragOverItem((prev) => (
          prev?.sectionKey === current.sectionKey && prev.itemId === itemId ? prev : { sectionKey: current.sectionKey, itemId }
        ));
      }
    } else if (target?.dataset.sidebarSection) {
      setDragOverSection(target.dataset.sidebarSection);
    }
  };

  const autoScrollTick = () => {
    const auto = autoScrollRef.current;
    const list = listRef.current;
    const current = pointerDragRef.current;
    if (!list || !current?.active || !auto.speed) {
      auto.frame = 0;
      return;
    }
    list.scrollTop += auto.speed;
    updateDropTarget(current, auto.x, auto.y);
    auto.frame = window.requestAnimationFrame(autoScrollTick);
  };

  /** Scroll the list while the pointer rests near its top or bottom edge. */
  const updateAutoScroll = (x: number, y: number) => {
    const auto = autoScrollRef.current;
    const list = listRef.current;
    auto.x = x;
    auto.y = y;
    if (!list) return;
    const rect = list.getBoundingClientRect();
    const depth = y < rect.top + DRAG_SCROLL_EDGE
      ? y - (rect.top + DRAG_SCROLL_EDGE)
      : y > rect.bottom - DRAG_SCROLL_EDGE
        ? y - (rect.bottom - DRAG_SCROLL_EDGE)
        : 0;
    auto.speed = Math.max(-DRAG_SCROLL_MAX_SPEED, Math.min(DRAG_SCROLL_MAX_SPEED, Math.round((depth / DRAG_SCROLL_EDGE) * DRAG_SCROLL_MAX_SPEED)));
    if (auto.speed && !auto.frame) auto.frame = window.requestAnimationFrame(autoScrollTick);
  };

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
        // A folder lifts together with its conversations; fall back to the header alone.
        const source = current.kind === "item"
          ? event.currentTarget.closest<HTMLElement>("[data-sidebar-item]")
          : event.currentTarget.closest<HTMLElement>("[data-sidebar-folder-group]")
            ?? event.currentTarget.closest<HTMLElement>("[data-cx-folder-header]");
        if (source) dragGhostRef.current = createDragGhost(source, current.startX, current.startY, reduced);
        document.documentElement.dataset.cxSidebarDragging = "";
        if (current.kind === "item") {
          setDraggedItem({ sectionKey: current.sectionKey, itemId: current.sourceId });
          setLiveMessage(`已拿起对话 ${current.label}，当前位置第 ${current.startIndex + 1} 项`);
        } else {
          setDraggedSection(current.sourceId);
          setLiveMessage(`已拿起项目 ${current.label}`);
        }
      }
      event.preventDefault();
      dragGhostRef.current?.move(event.clientX, event.clientY);
      updateAutoScroll(event.clientX, event.clientY);
      updateDropTarget(current, event.clientX, event.clientY);
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
      let moved = false;
      if (current.kind === "item") {
        const targetId = target?.dataset.sidebarItem;
        if (target?.dataset.sidebarSection === current.sectionKey && targetId && targetId !== current.sourceId) {
          current.reorder(current.sourceId, targetId);
          moved = true;
          setLiveMessage(`对话 ${current.label} 已移动到第 ${Number(target.dataset.sidebarIndex || "0") + 1} 项`);
        }
      } else {
        const targetId = target?.dataset.sidebarSection;
        if (targetId && targetId !== current.sourceId) {
          current.reorder(current.sourceId, targetId);
          moved = true;
          setLiveMessage(`项目 ${current.label} 已移动到 ${target?.dataset.sidebarLabel || "目标位置"} 之前`);
        }
      }
      if (moved) {
        dragGhostRef.current?.drop();
        dragGhostRef.current = null;
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

  const itemsById = useMemo(() => {
    const map = new Map<string, NavItem>();
    for (const section of [...sections, ...activitySections]) for (const item of section.items) map.set(item.id, item);
    return map;
  }, [sections, activitySections]);

  /* ── Multi-select ──────────────────────────────────────────────────────── */

  /** Thread ids in on-screen order (collapsed sections and hidden rails excluded). */
  const visibleThreadIds = useCallback((): string[] => {
    const ids: string[] = [];
    for (const row of Array.from(listRef.current?.querySelectorAll<HTMLElement>("[data-sidebar-item]") ?? [])) {
      const id = row.dataset.sidebarItem;
      if (!id || ids.includes(id) || !row.getClientRects().length || row.closest("[inert]")) continue;
      if (row.closest("[aria-hidden='true']")) continue;
      ids.push(id);
    }
    return ids;
  }, []);

  const clearSelection = useCallback(() => {
    selectionAnchorRef.current = null;
    setSelectedIds((current) => (current.size ? new Set() : current));
  }, []);

  // Drop ids that left the list (archived, filtered away).
  useEffect(() => {
    setSelectedIds((current) => {
      if (!current.size) return current;
      const next = new Set([...current].filter((id) => itemsById.has(id)));
      return next.size === current.size ? current : next;
    });
  }, [itemsById]);

  const onRowSelect = useCallback((id: string, event: React.MouseEvent<HTMLElement>) => {
    const toggle = event.metaKey || event.ctrlKey;
    if (toggle || event.shiftKey) {
      setHoverCard(null);
      setSelectedIds((current) => {
        const next = new Set(current);
        if (event.shiftKey) {
          const order = visibleThreadIds();
          const anchor = selectionAnchorRef.current ?? (activeId && order.includes(activeId) ? activeId : id);
          const from = order.indexOf(anchor);
          const to = order.indexOf(id);
          if (from >= 0 && to >= 0) {
            if (!toggle) next.clear();
            for (const rangeId of order.slice(Math.min(from, to), Math.max(from, to) + 1)) next.add(rangeId);
          } else {
            next.add(id);
          }
        } else {
          // The first ⌘-click also carries the open thread into the selection.
          if (!next.size && activeId && activeId !== id && itemsById.has(activeId)) next.add(activeId);
          if (next.has(id)) next.delete(id);
          else next.add(id);
          selectionAnchorRef.current = id;
        }
        setLiveMessage(`已选择 ${next.size} 个对话`);
        return next;
      });
      return;
    }
    clearSelection();
    onSelect(id);
  }, [activeId, clearSelection, itemsById, onSelect, visibleThreadIds]);

  useEffect(() => {
    if (!selectedIds.size) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
      if (document.querySelector("[data-cx-layer]")) return;
      event.preventDefault();
      clearSelection();
      setLiveMessage("已取消选择");
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [clearSelection, selectedIds.size]);

  /* ── Hover card ────────────────────────────────────────────────────────── */

  const onRowHover = useCallback((item: NavItem, element: HTMLElement | null) => {
    window.clearTimeout(hoverTimerRef.current);
    if (!element) {
      setHoverCard(null);
      return;
    }
    hoverTimerRef.current = window.setTimeout(() => {
      if (pointerDragRef.current?.active || !element.isConnected) return;
      setHoverCard({ item, element });
    }, HOVER_CARD_DELAY_MS);
  }, []);

  useEffect(() => () => window.clearTimeout(hoverTimerRef.current), []);
  const hoverSuppressed = Boolean(menu) || Boolean(renamingId) || Boolean(jumpHints) || Boolean(draggedItem) || Boolean(draggedSection) || Boolean(snoozeDialog);
  useEffect(() => {
    if (hoverSuppressed) {
      window.clearTimeout(hoverTimerRef.current);
      setHoverCard(null);
    }
  }, [hoverSuppressed]);
  const hoverItem = hoverCard ? itemsById.get(hoverCard.item.id) ?? hoverCard.item : null;

  /* ── ⌘1–9 jumps, ⌘⇧[ / ⌘⇧] previous / next ─────────────────────────────── */

  useEffect(() => {
    const mac = isMacPlatform();
    let timer: number | undefined;
    const clearHints = () => {
      window.clearTimeout(timer);
      timer = undefined;
      setJumpHints((current) => (current ? null : current));
    };
    const modifierHeld = (event: KeyboardEvent) => (mac ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey);
    const onKeyDown = (event: KeyboardEvent) => {
      const isModifierKey = event.key === (mac ? "Meta" : "Control");
      if (isModifierKey && !event.shiftKey && !event.altKey && !event.repeat) {
        window.clearTimeout(timer);
        timer = window.setTimeout(() => {
          const ids = visibleThreadIds().slice(0, 9);
          if (ids.length) setJumpHints(new Map(ids.map((id, index) => [id, index + 1])));
        }, JUMP_HINT_DELAY_MS);
        return;
      }
      if (!isModifierKey) clearHints();
      if (event.defaultPrevented || event.isComposing || !modifierHeld(event) || event.altKey) return;
      const digit = /^Digit([1-9])$/.exec(event.code);
      if (digit && !event.shiftKey) {
        const ids = visibleThreadIds();
        const target = ids[Number(digit[1]) - 1];
        if (!target) return;
        event.preventDefault();
        clearSelection();
        onSelect(target);
        return;
      }
      if (event.shiftKey && (event.code === "BracketLeft" || event.code === "BracketRight")) {
        const ids = visibleThreadIds();
        if (!ids.length) return;
        event.preventDefault();
        const current = activeId ? ids.indexOf(activeId) : -1;
        const step = event.code === "BracketLeft" ? -1 : 1;
        const next = current < 0 ? (step > 0 ? 0 : ids.length - 1) : (current + step + ids.length) % ids.length;
        clearSelection();
        onSelect(ids[next]);
      }
    };
    const onKeyUp = (event: KeyboardEvent) => {
      if (event.key === (mac ? "Meta" : "Control") || !modifierHeld(event)) clearHints();
    };
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
    window.addEventListener("blur", clearHints);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
      window.removeEventListener("blur", clearHints);
      window.clearTimeout(timer);
    };
  }, [activeId, clearSelection, onSelect, visibleThreadIds]);

  /* ── Snooze helpers ────────────────────────────────────────────────────── */

  const subjectOf = useCallback((ids: string[]) => (
    ids.length === 1 ? `「${itemsById.get(ids[0])?.label || "未命名对话"}」` : `${ids.length} 个对话`
  ), [itemsById]);

  const renderSnoozeItems = (ids: string[], apply: (until: number) => void) => (
    <>
      {snoozePresets().map((preset) => (
        <MenuItem key={preset.id} hint={formatWakeTime(preset.until)} onSelect={() => apply(preset.until)}>
          {preset.label}
        </MenuItem>
      ))}
      <MenuSeparator />
      <MenuItem icon="pencilLine" onSelect={() => setSnoozeDialog({ ids, subject: subjectOf(ids), apply })}>自定义时间…</MenuItem>
    </>
  );

  const copyText = (value: string, label: string) => {
    void copyToClipboard(value).then((ok) => {
      setLiveMessage(ok ? `已复制${label}` : "复制失败");
      toast({ title: ok ? `已复制${label}` : "复制失败，请手动复制", tone: ok ? "success" : "danger" });
    });
  };

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
      // Same targets as the ⌘ hints drawn on title rows; message hits only
      // take the digits when no title matches.
      const index = Number(event.key) - 1;
      const rows = visibleThreadIds();
      if (!rows.length && bodyHits.length > index) {
        event.preventDefault();
        const hit = bodyHits[index];
        onSelectBodyHit?.({ threadId: hit.threadId, messageId: hit.messageId });
        onSearchChange?.("");
        return;
      }
      const target = rows[index];
      if (!target) return;
      event.preventDefault();
      onSelect(target);
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
        const current = item.id === activeId;
        const filteringThis = Boolean(item.projectId && projectFilter?.value === item.projectId);
        return (
          <>
            {item.onNewInProject ? (
              <>
                <MenuItem icon="edit" onSelect={run(item.onNewInProject)}>在「{item.projectName}」中新建对话</MenuItem>
                <MenuSeparator />
              </>
            ) : null}
            {item.onPin ? (
              <MenuItem
                icon={item.pinned ? "pinOff" : "pin"}
                shortcut={current ? "mod+shift+p" : undefined}
                onSelect={run(() => {
                  setLiveMessage(item.pinned ? `已取消置顶 ${item.label}` : `已置顶 ${item.label}`);
                  item.onPin?.();
                })}
              >
                {item.pinned ? "取消置顶" : "置顶"}
              </MenuItem>
            ) : null}
            {item.onSettle ? (
              <MenuItem icon={item.settled ? "undo" : "checkCheck"} shortcut={current ? "mod+shift+s" : undefined} onSelect={run(item.onSettle)}>
                {item.settled ? "移回列表" : "归置"}
              </MenuItem>
            ) : null}
            {item.onSnooze ? (
              <>
                {item.snoozedUntil ? <MenuItem icon="bell" onSelect={run(() => item.onSnooze?.(null))}>立即唤醒</MenuItem> : null}
                <MenuSub icon="clock" label={item.snoozedUntil ? "更改提醒时间" : "稍后提醒"}>
                  {renderSnoozeItems([item.id], (until) => item.onSnooze?.(until))}
                </MenuSub>
              </>
            ) : null}
            <MenuSeparator />
            {item.onRenameCommit ? (
              <MenuItem icon="pencil" hint="双击" onSelect={run(() => setRenamingId(item.id))}>重命名</MenuItem>
            ) : item.onRename ? <MenuItem icon="pencil" onSelect={run(item.onRename)}>重命名</MenuItem> : null}
            {item.onFork ? <MenuItem icon="gitFork" onSelect={run(item.onFork)}>分叉对话</MenuItem> : null}
            {item.onFilterProject ? (
              <MenuItem icon="filter" onSelect={run(item.onFilterProject)}>
                {filteringThis ? "显示全部项目" : `只看「${item.projectName}」`}
              </MenuItem>
            ) : null}
            <MenuSub icon="copy" label="复制">
              {href ? <MenuItem icon="link" onSelect={() => copyText(absoluteHref(href), "对话链接")}>对话链接</MenuItem> : null}
              <MenuItem icon="hash" onSelect={() => copyText(item.id, "对话 ID")}>对话 ID</MenuItem>
              <MenuItem icon="quote" onSelect={() => copyText(item.label, "对话标题")}>对话标题</MenuItem>
              {item.projectPath ? <MenuItem icon="folder" onSelect={() => copyText(item.projectPath || "", "项目路径")}>项目路径</MenuItem> : null}
            </MenuSub>
            {href ? (
              <MenuItem icon="externalLink" onSelect={run(() => { window.open(href, "_blank", "noopener"); })}>
                在新标签页打开
              </MenuItem>
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
                <MenuItem icon="archive" danger onSelect={run(item.onArchive)}>归档</MenuItem>
              </>
            ) : null}
          </>
        );
      }
      case "snooze": {
        const { item } = target;
        return (
          <>
            <MenuLabel>稍后提醒</MenuLabel>
            {renderSnoozeItems([item.id], (until) => item.onSnooze?.(until))}
          </>
        );
      }
      case "bulk": {
        const { ids } = target;
        const allPinned = ids.every((id) => itemsById.get(id)?.pinned);
        const allSettled = ids.every((id) => itemsById.get(id)?.settled);
        const done = (message: string) => { clearSelection(); setLiveMessage(message); };
        return (
          <>
            <MenuLabel>已选择 {ids.length} 个对话</MenuLabel>
            {bulkActions ? (
              <>
                <MenuItem icon={allPinned ? "pinOff" : "pin"} onSelect={run(() => { bulkActions.pin(ids); done(allPinned ? "已取消置顶" : "已置顶"); })}>
                  {allPinned ? "取消置顶" : "全部置顶"}
                </MenuItem>
                <MenuItem icon={allSettled ? "undo" : "checkCheck"} onSelect={run(() => { bulkActions.settle(ids); done(allSettled ? "已移回列表" : "已归置"); })}>
                  {allSettled ? "移回列表" : "全部归置"}
                </MenuItem>
                <MenuSub icon="clock" label="稍后提醒">
                  {renderSnoozeItems(ids, (until) => { bulkActions.snooze(ids, until); done("已设置稍后提醒"); })}
                </MenuSub>
              </>
            ) : null}
            <MenuItem icon="copy" onSelect={run(() => copyText(ids.join("\n"), "对话 ID"))}>复制对话 ID</MenuItem>
            <MenuItem icon="x" shortcut="esc" onSelect={run(() => done("已取消选择"))}>取消选择</MenuItem>
            {bulkActions?.archive ? (
              <>
                <MenuSeparator />
                <MenuItem icon="archive" danger onSelect={run(() => { bulkActions.archive?.(ids); done("正在归档所选对话"); })}>
                  归档 {ids.length} 个对话
                </MenuItem>
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
  const isFooterSection = (section: NavSection) => section.kind === "snoozed" || section.kind === "settled";
  const footerSections = activityView ? [] : visibleSections.filter(isFooterSection);
  const otherSections = activityView
    ? visibleSections
    : visibleSections.filter((section) => section.kind !== "pinned" && section.kind !== "folder" && !isFooterSection(section));

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
            snoozeOpen={menu?.key === `snooze:${sectionKey}:${item.id}`}
            selected={selectedIds.has(item.id)}
            jumpHint={jumpHints?.get(item.id)}
            renaming={renamingId === item.id}
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
            onSelect={onRowSelect}
            onStartRename={(target) => setRenamingId(target.id)}
            onRenameDone={() => {
              const id = renamingId;
              setRenamingId(null);
              if (id) refocusItem(id);
            }}
            onHover={onRowHover}
            onOpenSnooze={(target, anchor) => openMenuFromButton(
              `snooze:${sectionKey}:${target.id}`,
              { kind: "snooze", item: target },
              anchor,
              rowFocusTarget(anchor.closest("[data-sidebar-item]")),
            )}
            onOpenMenu={(target, anchor) => openMenuFromButton(
              `item:${sectionKey}:${target.id}`,
              { kind: "item", item: target, section },
              anchor,
              rowFocusTarget(anchor.closest("[data-sidebar-item]")),
            )}
            onContextMenu={(event, target) => {
              if (selectedIds.size > 1 && selectedIds.has(target.id)) {
                openMenuAtPoint(event, "bulk", { kind: "bulk", ids: [...selectedIds] }, rowFocusTarget(event.currentTarget));
                return;
              }
              openMenuAtPoint(
                event,
                `item:${sectionKey}:${target.id}`,
                { kind: "item", item: target, section },
                rowFocusTarget(event.currentTarget),
              );
            }}
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
      <div
        key={sectionKey}
        data-sidebar-folder-group=""
        className={cn("flex flex-col transition-opacity duration-150", draggedSection === sectionKey && "opacity-45")}
      >
        <FolderHeader
          section={section}
          sectionKey={sectionKey}
          collapsed={collapsed}
          count={section.items.length}
          menuOpen={menu?.key === menuKey}
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
          <div id={bodyId} role="group" aria-label={section.title} className="flex flex-col gap-0.5 pb-1.5">
            {!section.items.length && section.emptyLabel ? (
              <p className="flex h-8 items-center pl-[30px] text-[13px] text-cx-fg-4">{section.emptyLabel}</p>
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
                className="cx-press flex h-8 items-center gap-1 rounded-lg pl-[30px] text-left text-[13px] text-cx-fg-3 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
              >
                {preview.expanded ? "收起" : `展开其余 ${preview.overflow} 个`}
              </button>
            ) : null}
          </div>
        </Collapse>
      </div>
    );
  };

  const activityMenu = (
    <Menu
      placement="bottom-end"
      ariaLabel="活动视图选项"
      trigger={<IconButton icon="more" label="活动视图选项" size="sm" className="rounded-md" />}
    >
      {activityOptions ? (
        <>
          <MenuLabel>显示</MenuLabel>
          {([
            ["showPriority", "优先事项部分", "查看后仍保留，清除已读对话时移除"],
            ["showRunning", "进行中", "将正在执行的对话加入优先级"],
            ["showPinned", "置顶", "将置顶对话单独分组显示"],
          ] as const).map(([key, label, description]) => (
            <MenuItem
              key={key}
              checked={activityOptions[key]}
              keepOpen
              description={description}
              onSelect={() => activityOptions.onShowChange(key, !activityOptions[key])}
            >
              {label}
            </MenuItem>
          ))}
          <MenuSeparator />
          <MenuItem
            icon="checkCheck"
            shortcut="shift+esc"
            disabled={!markAllRead}
            onSelect={() => markAllRead?.()}
          >
            全部标为已读
          </MenuItem>
          <MenuItem
            icon="x"
            disabled={!activityOptions.readCount}
            onSelect={activityOptions.onClearRead}
          >
            清除已读对话
          </MenuItem>
          <MenuItem icon="refresh" onSelect={activityOptions.onRestoreDefaults}>恢复默认</MenuItem>
          <MenuSeparator />
        </>
      ) : null}
      {onToggleActivityView ? (
        <MenuItem shortcut="alt+mod+u" onSelect={onToggleActivityView}>关闭活动视图</MenuItem>
      ) : null}
    </Menu>
  );

  const renderPlainSection = (section: NavSection) => {
    const sectionKey = sectionKeyOf(section);
    const isPinned = section.kind === "pinned";
    const isActivity = section.kind === "activity";
    if (!section.items.length && !(isActivity && section.emptyLabel)) return null;
    // Default-collapsed sections remember an explicit "open" toggle instead.
    const toggleKey = section.defaultCollapsed ? `${sectionKey}:open` : sectionKey;
    const collapsed = !searching && (section.defaultCollapsed ? !collapsedIds.has(toggleKey) : collapsedIds.has(toggleKey));
    const bodyId = `sidebar-section-${sectionKey}`;
    const menuKey = `pinned:${sectionKey}`;
    const pageLimit = section.pageSize ? pageLimits[sectionKey] ?? section.pageSize : null;
    const shownItems = pageLimit !== null && !searching ? section.items.slice(0, pageLimit) : section.items;
    const hiddenCount = section.items.length - shownItems.length;
    const receded = section.kind === "settled" || section.kind === "snoozed";
    return (
      <section key={sectionKey} className={cn("mt-3 flex flex-col first:mt-0", receded && "mt-0.5")} aria-label={section.title}>
        <SectionLabel
          title={(
            <span className="flex items-center gap-1.5">
              {section.kind === "settled" ? <Icon name="checkCheck" size={12} /> : section.kind === "snoozed" ? <Icon name="clock" size={12} /> : null}
              {section.title}
            </span>
          )}
          collapsed={collapsed}
          controlsId={bodyId}
          onToggle={() => toggleSection(toggleKey)}
          actions={section.id === "activity-priority" ? activityMenu : isPinned && section.onSortChange ? (
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
          <div id={bodyId} role="group" aria-label={section.title} className="flex flex-col gap-0.5">
            {!section.items.length && section.emptyLabel ? (
              <p className="px-2.5 py-1.5 text-[13px] text-cx-fg-4">{section.emptyLabel}</p>
            ) : null}
            {renderRows(section, sectionKey, shownItems, false)}
            {hiddenCount > 0 ? (
              <button
                type="button"
                data-cx-nav-row=""
                onClick={() => setPageLimits((current) => ({ ...current, [sectionKey]: shownItems.length + (section.pageStep || 25) }))}
                className="cx-press flex h-8 items-center gap-1 rounded-lg pl-2.5 text-left text-[13px] text-cx-fg-3 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
              >
                再显示 {Math.min(hiddenCount, section.pageStep || 25)} 个
                <span className="text-cx-fg-4">（共 {section.items.length} 个）</span>
              </button>
            ) : null}
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

      <div className="flex flex-none flex-col gap-2 px-3 pb-3 pt-2">
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
            {onToggleActivityView ? (
              <IconButton
                icon="bell"
                label={activityBadge > 0 ? `查看活动，${activityBadge} 项需要关注` : "查看活动"}
                tooltip={activityView ? "关闭活动视图" : "查看活动"}
                shortcut="alt+mod+u"
                size="md"
                active={activityView}
                aria-controls="cx-sidebar-list"
                onClick={onToggleActivityView}
                className="rounded-lg"
                badge={activityBadge > 0 ? (activityBadge > 99 ? "99+" : activityBadge) : undefined}
              />
            ) : null}
            {projectFilter && projectFilter.options.length ? (
              <Menu
                placement="bottom-end"
                ariaLabel="按项目筛选"
                className="w-[260px]"
                trigger={(
                  <IconButton
                    icon="filter"
                    label={projectFilter.value ? "按项目筛选（已启用）" : "按项目筛选"}
                    size="md"
                    active={Boolean(projectFilter.value)}
                    className="rounded-lg"
                  />
                )}
              >
                <MenuLabel>按项目筛选</MenuLabel>
                <MenuItem icon="messages" checked={!projectFilter.value} onSelect={() => projectFilter.onChange(null)}>全部项目</MenuItem>
                <MenuSeparator />
                {projectFilter.options.map((option) => (
                  <MenuItem
                    key={option.id}
                    icon="folder"
                    checked={projectFilter.value === option.id}
                    hint={option.count ? String(option.count) : undefined}
                    description={option.path}
                    onSelect={() => projectFilter.onChange(projectFilter.value === option.id ? null : option.id)}
                  >
                    {option.name}
                  </MenuItem>
                ))}
              </Menu>
            ) : null}
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
        {projectFilter?.value && !activityView ? (
          <div className="flex h-7 items-center gap-1.5 rounded-lg bg-cx-hover pl-2 pr-1 text-[12.5px] text-cx-fg-2">
            <Icon name="filter" size={13} className="shrink-0 text-cx-fg-3" />
            <span className="min-w-0 flex-1 truncate">
              仅显示「{projectFilter.options.find((option) => option.id === projectFilter.value)?.name || "项目"}」
            </span>
            <button
              type="button"
              aria-label="清除项目筛选"
              onClick={() => projectFilter.onChange(null)}
              className="cx-press grid size-5 place-items-center rounded-md text-cx-fg-3 hover:bg-cx-active hover:text-cx-fg"
            >
              <Icon name="x" size={12} />
            </button>
          </div>
        ) : null}
      </div>

      <ScrollArea
        ref={listRef}
        id="cx-sidebar-list"
        className="flex-1 overscroll-contain px-3 pb-4"
        onKeyDown={onListKeyDown}
        onPointerDownCapture={() => {
          window.clearTimeout(hoverTimerRef.current);
          setHoverCard(null);
        }}
        onScroll={() => {
          if (hoverCard) setHoverCard(null);
        }}
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

        {activityView && !activitySections.some((section) => section.id === "activity-priority") ? (
          <div className="flex h-8 items-center justify-between pl-2.5">
            <span className="text-[13px] font-medium text-cx-fg-2">活动</span>
            {activityMenu}
          </div>
        ) : null}

        {showSkeleton ? <SkeletonRows /> : null}

        {pinnedSections.map(renderPlainSection)}

        {showProjects ? (
          <section className="mt-3 flex flex-col first:mt-0" aria-label="项目">
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
              <div id="sidebar-projects" className="flex flex-col gap-0.5">
                {visibleFolders.map(renderFolder)}
                {!visibleFolders.length && onCreateFolder ? (
                  <button
                    type="button"
                    data-cx-nav-row=""
                    onClick={onCreateFolder}
                    className="cx-press flex h-8 items-center gap-2 rounded-lg pl-2.5 text-left text-[14px] text-cx-fg-3 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
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
              title="还没有对话"
              description="开始一段新对话，它会出现在这里。"
            />
          )
        ) : null}
      </ScrollArea>

      {footerSections.length ? (
        <div className="flex max-h-[45%] flex-none flex-col border-t border-cx-border-subtle px-3 pb-1.5 pt-1.5" data-cx-sidebar-footer="">
          <ScrollArea className="min-h-0 flex-1 overscroll-contain">
            {footerSections.map(renderPlainSection)}
          </ScrollArea>
        </div>
      ) : null}

      <AnimatePresence initial={false}>
        {selectedIds.size ? (
          <motion.div
            key="bulk-bar"
            initial={reduced ? { opacity: 0 } : { opacity: 0, y: 8 }}
            animate={{ opacity: 1, y: 0, transition: { duration: 0.16, ease: [0.16, 1, 0.3, 1] } }}
            exit={{ opacity: 0, transition: { duration: 0.1 } }}
            role="toolbar"
            aria-label={`已选择 ${selectedIds.size} 个对话`}
            className="mx-2 mb-2 flex flex-none items-center gap-0.5 rounded-xl border border-cx-border-subtle bg-cx-overlay p-1 shadow-cx-pop"
          >
            <span className="min-w-0 flex-1 truncate pl-2 text-[12.5px] font-medium text-cx-fg-2">已选 {selectedIds.size} 个</span>
            {bulkActions ? (
              <>
                <IconButton
                  icon={[...selectedIds].every((id) => itemsById.get(id)?.pinned) ? "pinOff" : "pin"}
                  label="置顶所选对话"
                  size="sm"
                  onClick={() => { bulkActions.pin([...selectedIds]); clearSelection(); }}
                />
                <IconButton
                  icon="checkCheck"
                  label="归置所选对话"
                  size="sm"
                  onClick={() => { bulkActions.settle([...selectedIds]); clearSelection(); }}
                />
                <Menu
                  placement="top-end"
                  ariaLabel="稍后提醒所选对话"
                  trigger={<IconButton icon="clock" label="稍后提醒所选对话" size="sm" />}
                >
                  <MenuLabel>稍后提醒 {selectedIds.size} 个对话</MenuLabel>
                  {renderSnoozeItems([...selectedIds], (until) => { bulkActions.snooze([...selectedIds], until); clearSelection(); })}
                </Menu>
                {bulkActions.archive ? (
                  <IconButton
                    icon="archive"
                    label="归档所选对话"
                    size="sm"
                    onClick={() => { bulkActions.archive?.([...selectedIds]); clearSelection(); }}
                  />
                ) : null}
              </>
            ) : null}
            <IconButton icon="x" label="取消选择" shortcut="esc" size="sm" onClick={clearSelection} />
          </motion.div>
        ) : null}
      </AnimatePresence>

      <ThreadHoverCard item={hoverSuppressed ? null : hoverItem} anchor={hoverSuppressed ? null : hoverCard?.element ?? null} />

      <SnoozeDialog
        open={Boolean(snoozeDialog)}
        subject={snoozeDialog?.subject || ""}
        onOpenChange={(open) => { if (!open) setSnoozeDialog(null); }}
        onConfirm={(until) => snoozeDialog?.apply(until)}
      />

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
          <MenuScope close={close}>
            <div className="cx-scroll flex min-h-0 flex-col overflow-y-auto overflow-x-hidden">{renderMenu(renderedMenu, close)}</div>
          </MenuScope>
        ) : null)}
      </Popover>
    </nav>
  );
}
