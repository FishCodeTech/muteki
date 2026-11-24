"use client";

import React, { useMemo, useRef, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import {
  countPanes,
  MAX_PANES,
  paneLayoutStore,
  usePaneLayout,
  type PaneLeaf,
  type PaneLayoutPreset,
  type PaneNode,
  type PaneSplit,
} from "@/lib/paneLayoutStore";
import {
  Button,
  IconButton,
  Input,
  Menu,
  MenuItem,
  MenuSeparator,
  Tooltip,
} from "@muteki/ui";
import { Icon } from "@muteki/ui";

export interface PaneThreadOption {
  thread_id: string;
  title?: string;
  project_name?: string;
  updated_at?: string;
}

export interface PaneChildContext {
  paneId: string;
  isActive: boolean;
  isPrimary: boolean;
  isZoomed: boolean;
  paneCount: number;
  onFocus: () => void;
  onSplitRight: () => void;
  onSplitDown: () => void;
  onToggleZoom: () => void;
  onClose: () => void;
  onSelectThread: (threadId: string) => void;
}

export function PaneLayoutMenu({
  paneId,
  currentThreadId,
  delegatedThreadIds = [],
  compact = false,
}: {
  paneId: string;
  currentThreadId: string;
  delegatedThreadIds?: string[];
  compact?: boolean;
}) {
  const layout = usePaneLayout();
  const paneCount = countPanes(layout.root);
  const canSplit = paneCount < MAX_PANES;
  const isZoomed = layout.zoomedPaneId === paneId;

  const apply = (preset: PaneLayoutPreset) => {
    paneLayoutStore.applyPreset(preset, currentThreadId, delegatedThreadIds);
  };

  return (
    <Menu
      trigger={(
        <IconButton
          icon="columns"
          label={paneCount > 1 ? `分屏布局（${paneCount}/${MAX_PANES}）` : "分屏与布局"}
          active={paneCount > 1}
          size={compact ? "xs" : "sm"}
          data-testid={`pane-layout-menu-${paneId}`}
        />
      )}
    >
      <MenuItem
        icon="split"
        shortcut="⌘\\"
        disabled={!canSplit}
        onSelect={() => { paneLayoutStore.splitPane(paneId, "horizontal"); }}
      >
        向右分屏
      </MenuItem>
      <MenuItem
        icon="panelBottom"
        shortcut="⌘⇧\\"
        disabled={!canSplit}
        onSelect={() => { paneLayoutStore.splitPane(paneId, "vertical"); }}
      >
        向下分屏
      </MenuItem>
      {paneCount > 1 ? (
        <>
          <MenuItem
            icon={isZoomed ? "minimize" : "maximize"}
            shortcut="⌘⇧Z"
            onSelect={() => paneLayoutStore.toggleZoomPane(paneId)}
          >
            {isZoomed ? "还原窗格" : "放大当前窗格"}
          </MenuItem>
          {!paneLayoutStore.isPrimaryPane(paneId) ? (
            <MenuItem
              icon="x"
              onSelect={() => paneLayoutStore.closePane(paneId)}
            >
              关闭当前窗格
            </MenuItem>
          ) : null}
          <MenuItem
            icon="panel"
            onSelect={() => paneLayoutStore.closeOtherPanes(paneId)}
          >
            关闭其他窗格
          </MenuItem>
        </>
      ) : null}
      <MenuSeparator />
      <MenuItem icon="panel" onSelect={() => apply("single")}>
        单窗格
      </MenuItem>
      <MenuItem icon="columns" onSelect={() => apply("columns")}>
        双列并排
      </MenuItem>
      <MenuItem icon="panel" onSelect={() => apply("one_beside_two")}>
        左一右二
      </MenuItem>
      <MenuItem icon="grid" onSelect={() => apply("grid")}>
        四宫格
      </MenuItem>
      <MenuItem
        icon="user"
        disabled={delegatedThreadIds.length === 0}
        onSelect={() => apply("spread_agents")}
      >
        平铺委派线程{delegatedThreadIds.length > 0 ? ` (${delegatedThreadIds.length})` : ""}
      </MenuItem>
    </Menu>
  );
}

function WaitingPanePicker({
  paneId,
  threads,
  onSelectThread,
  onClose,
}: {
  paneId: string;
  threads: PaneThreadOption[];
  onSelectThread: (threadId: string) => void;
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    const list = threads.filter((t) => t.thread_id);
    if (!q) return list.slice(0, 24);
    return list
      .filter((t) =>
        (t.title || "").toLowerCase().includes(q) ||
        (t.project_name || "").toLowerCase().includes(q) ||
        t.thread_id.toLowerCase().includes(q),
      )
      .slice(0, 24);
  }, [query, threads]);

  return (
    <div
      className="flex h-full min-h-0 flex-col bg-cx-bg"
      data-testid={`waiting-pane-${paneId}`}
    >
      <div className="flex h-11 shrink-0 items-center justify-between border-b border-cx-border px-3">
        <div className="flex items-center gap-2 text-cx-sm font-medium text-cx-fg">
          <Icon name="columns" size={14} className="text-cx-fg-3" />
          <span>选择要在该窗格打开的对话</span>
        </div>
        <Tooltip content="关闭此窗格">
          <IconButton icon="x" label="关闭此窗格" size="xs" onClick={onClose} />
        </Tooltip>
      </div>
      <div className="flex min-h-0 flex-1 flex-col items-center justify-center p-6">
        <div className="w-full max-w-md rounded-xl ring-1 ring-cx-ring bg-cx-elevated p-4 shadow-cx-sm">
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="搜索对话标题或 ID，或在左侧栏点击任意对话…"
            aria-label="搜索对话"
            className="mb-3"
            autoFocus
          />
          <div className="max-h-64 space-y-1 overflow-y-auto">
            {filtered.length === 0 ? (
              <p className="py-6 text-center text-cx-xs text-cx-fg-4">
                暂无可选对话，请直接在左侧栏点击任意对话
              </p>
            ) : (
              filtered.map((item) => (
                <button
                  key={item.thread_id}
                  type="button"
                  onClick={() => onSelectThread(item.thread_id)}
                  className="flex w-full items-center justify-between gap-2 rounded-lg px-2.5 py-2 text-left text-cx-sm text-cx-fg transition-colors hover:bg-cx-hover"
                >
                  <span className="min-w-0 truncate font-medium">
                    {item.title || item.thread_id}
                  </span>
                  {item.project_name ? (
                    <span className="shrink-0 rounded bg-cx-hover px-1.5 py-0.5 text-cx-2xs text-cx-fg-3">
                      {item.project_name}
                    </span>
                  ) : null}
                </button>
              ))
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function SplitContainer({
  node,
  renderLeaf,
}: {
  node: PaneSplit;
  renderLeaf: (leaf: PaneLeaf) => ReactNode;
}) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const horizontal = node.axis === "horizontal";

  const startResize = (event: React.MouseEvent) => {
    event.preventDefault();
    const container = containerRef.current;
    if (!container) return;
    const rect = container.getBoundingClientRect();
    const total = horizontal ? rect.width : rect.height;
    if (total <= 0) return;

    const onMove = (moveEvent: MouseEvent) => {
      const pos = horizontal
        ? moveEvent.clientX - rect.left
        : moveEvent.clientY - rect.top;
      paneLayoutStore.setSplitRatio(node.id, pos / total);
    };
    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };

  const firstPct = `${(node.ratio * 100).toFixed(3)}%`;
  const secondPct = `${((1 - node.ratio) * 100).toFixed(3)}%`;

  return (
    <div
      ref={containerRef}
      className={cn(
        "flex min-h-0 min-w-0 flex-1 overflow-hidden",
        horizontal ? "flex-row" : "flex-col",
      )}
    >
      <div
        className="flex min-h-0 min-w-0 overflow-hidden"
        style={{ flexBasis: firstPct, flexGrow: 0, flexShrink: 0 }}
      >
        <PaneTreeNode node={node.first} renderLeaf={renderLeaf} />
      </div>
      <div
        role="separator"
        aria-orientation={horizontal ? "vertical" : "horizontal"}
        onMouseDown={startResize}
        className={cn(
          "shrink-0 bg-cx-border transition-colors hover:bg-cx-accent/60",
          horizontal ? "w-[3px] cursor-col-resize" : "h-[3px] cursor-row-resize",
        )}
      />
      <div
        className="flex min-h-0 min-w-0 overflow-hidden"
        style={{ flexBasis: secondPct, flexGrow: 0, flexShrink: 0 }}
      >
        <PaneTreeNode node={node.second} renderLeaf={renderLeaf} />
      </div>
    </div>
  );
}

function PaneTreeNode({
  node,
  renderLeaf,
}: {
  node: PaneNode;
  renderLeaf: (leaf: PaneLeaf) => ReactNode;
}) {
  if (node.kind === "leaf") {
    return <>{renderLeaf(node)}</>;
  }
  return <SplitContainer node={node} renderLeaf={renderLeaf} />;
}

export function ConversationPaneWorkspace({
  primaryPane,
  threads,
  renderSecondaryPane,
}: {
  primaryPane: ReactNode;
  threads: PaneThreadOption[];
  renderSecondaryPane: (leaf: PaneLeaf, ctx: PaneChildContext) => ReactNode;
}) {
  const layout = usePaneLayout();
  const paneCount = countPanes(layout.root);

  if (paneCount <= 1) {
    return <>{primaryPane}</>;
  }

  const renderLeaf = (leaf: PaneLeaf) => {
    const isPrimary = paneLayoutStore.isPrimaryPane(leaf.id);
    const isActive = layout.activePaneId === leaf.id;
    const isZoomed = layout.zoomedPaneId === leaf.id;
    const hiddenBeneathZoom = Boolean(layout.zoomedPaneId && !isZoomed);

    const ctx: PaneChildContext = {
      paneId: leaf.id,
      isActive,
      isPrimary,
      isZoomed,
      paneCount,
      onFocus: () => paneLayoutStore.setActivePane(leaf.id),
      onSplitRight: () => paneLayoutStore.splitPane(leaf.id, "horizontal"),
      onSplitDown: () => paneLayoutStore.splitPane(leaf.id, "vertical"),
      onToggleZoom: () => paneLayoutStore.toggleZoomPane(leaf.id),
      onClose: () => paneLayoutStore.closePane(leaf.id),
      onSelectThread: (threadId) => paneLayoutStore.setPaneThread(leaf.id, threadId),
    };

    return (
      <div
        key={leaf.id}
        data-pane-id={leaf.id}
        data-pane-active={isActive ? "true" : "false"}
        onMouseDownCapture={() => paneLayoutStore.setActivePane(leaf.id)}
        className={cn(
          "relative flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden bg-cx-bg",
          isActive && "ring-1 ring-inset ring-cx-accent/55",
          isZoomed && "fixed inset-0 z-30 md:static md:absolute md:inset-0 md:z-30",
          hiddenBeneathZoom && "pointer-events-none",
        )}
      >
        {isZoomed ? (
          <div className="flex h-8 shrink-0 items-center justify-between border-b border-cx-border bg-cx-elevated px-3 text-cx-xs text-cx-fg-2">
            <span>已放大当前窗格</span>
            <Button
              size="xs"
              variant="ghost"
              icon="minimize"
              onClick={() => paneLayoutStore.unzoom()}
            >
              还原 (Esc)
            </Button>
          </div>
        ) : null}
        {isPrimary
          ? primaryPane
          : leaf.threadId
            ? renderSecondaryPane(leaf, ctx)
            : (
              <WaitingPanePicker
                paneId={leaf.id}
                threads={threads}
                onSelectThread={(id) => paneLayoutStore.setPaneThread(leaf.id, id)}
                onClose={() => paneLayoutStore.closePane(leaf.id)}
              />
            )}
      </div>
    );
  };

  return (
    <div
      className="relative flex min-h-0 min-w-0 flex-1 overflow-hidden"
      data-testid="conversation-pane-workspace"
      data-pane-count={paneCount}
    >
      <PaneTreeNode node={layout.root} renderLeaf={renderLeaf} />
    </div>
  );
}
