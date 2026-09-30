"use client";

import { useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { focusTargetAfterTabClose, handleTabListKeyDown, isTabId } from "@/components/collaboration/tablist";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import {
  IconButton,
  Kbd,
  Menu,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  ResizeHandle,
  ScrollArea,
  Sheet,
  toast,
  useContextMenu,
} from "@/components/chat/ui";
import {
  PANEL_WIDTH_DEFAULT,
  PANEL_WIDTH_MIN,
  chatPanel,
  panelWidthMax,
  surfaceMeta,
  surfaceTitle,
  useChatPanel,
  useChatPanelWidth,
  type ChatSurface,
  type SingletonKind,
} from "@/lib/chatPanelStore";
import type { SurfaceContext } from "./types";
import { SurfaceRenderer } from "./SurfaceRenderer";

interface LauncherItem {
  kind: SingletonKind | "preview";
  label: string;
  description: string;
  icon: IconName;
  key: string;
  needsWorkspace?: boolean;
}

const LAUNCHER: LauncherItem[] = [
  { kind: "diff", label: "变更", description: "按回合或工作树审查代码改动", icon: "gitCompare", key: "D", needsWorkspace: true },
  { kind: "preview", label: "预览", description: "在面板内打开本地服务或网页", icon: "globe", key: "B" },
  { kind: "files", label: "文件", description: "浏览、搜索和预览工作区文件", icon: "folderTree", key: "F", needsWorkspace: true },
  { kind: "terminal", label: "终端", description: "在工作区打开持久交互终端", icon: "terminal", key: "T", needsWorkspace: true },
  { kind: "overview", label: "概览", description: "运行状态、后台进程与产物", icon: "layers", key: "O" },
  { kind: "plan", label: "计划", description: "Agent 上报的计划步骤与进度", icon: "listTodo", key: "P" },
  { kind: "agents", label: "Agents", description: "委派 Agent 的层级与执行归属", icon: "bot", key: "A" },
  { kind: "pull-request", label: "Pull request", description: "当前分支关联的 Pull request", icon: "gitPullRequest", key: "R", needsWorkspace: true },
];

function openLauncherItem(threadId: string, item: LauncherItem) {
  if (item.kind === "preview") chatPanel.openPreview(threadId, null, { newTab: true });
  else chatPanel.open(threadId, item.kind);
}

function surfaceIcon(surface: ChatSurface): IconName {
  if (surface.kind === "file") return "fileCode";
  return surfaceMeta(surface.kind).icon as IconName;
}

function Favicon({ url }: { url: string | null }) {
  const [failed, setFailed] = useState(false);
  const src = useMemo(() => {
    if (!url) return null;
    try {
      const parsed = new URL(url);
      if (/^(localhost|127\.0\.0\.1|\[::1\])$/.test(parsed.hostname)) return null;
      return `${parsed.origin}/favicon.ico`;
    } catch {
      return null;
    }
  }, [url]);
  if (!src || failed) return <Icon name="globe" size={13} />;
  // eslint-disable-next-line @next/next/no-img-element
  return <img src={src} alt="" width={13} height={13} className="size-[13px] rounded-[3px]" onError={() => setFailed(true)} />;
}
const WORKSPACE_TAB_ID_PREFIX = "workspace-surface-tab";
const OPEN_VIEW_CONTROL_ID = "workspace-surface-open-view";

function SurfaceTab({
  threadId,
  surface,
  active,
  tabIndex,
  onClose,
  onDragStart,
  onDrop,
}: {
  threadId: string;
  surface: ChatSurface;
  active: boolean;
  tabIndex: number;
  onClose: (id: string) => void;
  onDragStart: (id: string) => void;
  onDrop: (id: string) => void;
}) {
  const ctx = useContextMenu();
  const title = surfaceTitle(surface);
  const ref = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (active) ref.current?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [active, surface.id, threadId]);
  return (
    <>
      <div
        ref={ref}
        id={`${WORKSPACE_TAB_ID_PREFIX}-${surface.id}`}
        role="tab"
        aria-selected={active}
        aria-controls={`workspace-surface-panel-${surface.id}`}
        tabIndex={tabIndex}
        draggable
        title={surface.kind === "file" ? surface.path : surface.kind === "preview" ? surface.url ?? title : title}
        onDragStart={(event) => { event.dataTransfer.effectAllowed = "move"; onDragStart(surface.id); }}
        onDragOver={(event) => { event.preventDefault(); event.dataTransfer.dropEffect = "move"; }}
        onDrop={(event) => { event.preventDefault(); onDrop(surface.id); }}
        onClick={() => chatPanel.activate(threadId, surface.id)}
        onFocus={() => ref.current?.scrollIntoView({ block: "nearest", inline: "nearest" })}
        onAuxClick={(event) => { if (event.button === 1) { event.preventDefault(); onClose(surface.id); } }}
        onContextMenu={ctx.onContextMenu}
        className={cn(
          "group/tab relative flex h-7 max-w-[180px] shrink-0 cursor-default select-none items-center gap-1.5 rounded-lg pl-2 pr-1 text-[12.5px] font-medium outline-none transition-colors duration-150",
          active
            ? "bg-cx-elevated text-cx-fg shadow-[0_0_0_1px_var(--cx-border),0_1px_2px_hsl(var(--cx-shadow-color)/0.06)]"
            : "text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg",
          "focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
        )}
      >
        <span className={cn("grid size-4 shrink-0 place-items-center", active ? "text-cx-fg-2" : "text-cx-fg-4")}>
          {surface.kind === "preview" ? <Favicon url={surface.url} /> : <Icon name={surfaceIcon(surface)} size={13} />}
        </span>
        <span className="min-w-0 truncate">{title}</span>
        <button
          type="button"
          tabIndex={-1}
          aria-label={`关闭 ${title}`}
          onClick={(event) => { event.stopPropagation(); onClose(surface.id); }}
          className={cn(
            "grid size-5 shrink-0 place-items-center rounded-md text-cx-fg-4 transition-opacity hover:bg-cx-active hover:text-cx-fg",
            active ? "opacity-100" : "opacity-0 group-hover/tab:opacity-100",
          )}
        >
          <Icon name="x" size={11} />
        </button>
      </div>
      {ctx.render(
        <>
          <MenuItem icon="x" onSelect={() => onClose(surface.id)}>关闭</MenuItem>
          <MenuItem onSelect={() => chatPanel.closeOthers(threadId, surface.id)}>关闭其他</MenuItem>
          <MenuItem onSelect={() => chatPanel.closeToRight(threadId, surface.id)}>关闭右侧</MenuItem>
          <MenuItem onSelect={() => chatPanel.closeAll(threadId)}>全部关闭</MenuItem>
          {surface.kind === "file" || (surface.kind === "preview" && surface.url) ? (
            <>
              <MenuSeparator />
              <MenuItem
                icon="copy"
                onSelect={() => {
                  void navigator.clipboard.writeText(surface.kind === "file" ? surface.path : surface.url || "");
                  toast({ title: surface.kind === "file" ? "已复制路径" : "已复制地址", tone: "success", duration: 1800 });
                }}
              >
                {surface.kind === "file" ? "复制路径" : "复制地址"}
              </MenuItem>
            </>
          ) : null}
        </>,
        "标签操作",
      )}
    </>
  );
}

function AddSurfaceMenu({ threadId, hasWorkspace }: { threadId: string; hasWorkspace: boolean }) {
  return (
    <Menu
      placement="bottom-start"
      ariaLabel="打开工作视图"
      trigger={<IconButton id={OPEN_VIEW_CONTROL_ID} icon="plus" label="打开视图" size="xs" className="ml-0.5" />}
    >
      <MenuLabel>打开视图</MenuLabel>
      {LAUNCHER.map((item) => (
        <MenuItem
          key={item.kind}
          icon={item.icon}
          disabled={item.needsWorkspace && !hasWorkspace}
          hint={<Kbd tone="subtle">{item.key}</Kbd>}
          onSelect={() => openLauncherItem(threadId, item)}
        >
          {item.label}
        </MenuItem>
      ))}
    </Menu>
  );
}

function Launcher({ threadId, hasWorkspace }: { threadId: string; hasWorkspace: boolean }) {
  return (
    <div className="cx-scroll flex h-full flex-col overflow-y-auto px-5 pb-6 pt-8">
      <div className="mb-5">
        <h2 className="text-[15px] font-semibold tracking-[-0.01em] text-cx-fg">打开工作视图</h2>
        <p className="mt-1 text-[12.5px] text-cx-fg-3">在右侧面板并排查看变更、预览、文件和终端。按字母键快速打开。</p>
      </div>
      <div className="grid grid-cols-1 gap-1.5 @[440px]/panel:grid-cols-2">
        {LAUNCHER.map((item) => {
          const unavailable = Boolean(item.needsWorkspace && !hasWorkspace);
          return (
            <button
              key={item.kind}
              type="button"
              disabled={unavailable}
              onClick={() => openLauncherItem(threadId, item)}
              className="cx-press group flex items-start gap-3 rounded-xl border border-transparent p-3 text-left transition-colors hover:border-cx-border hover:bg-cx-elevated hover:shadow-cx-xs disabled:cursor-not-allowed disabled:opacity-45"
            >
              <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-cx-hover text-cx-fg-2 transition-colors group-hover:bg-cx-accent-soft group-hover:text-cx-accent">
                <Icon name={item.icon} size={16} />
              </span>
              <span className="flex min-w-0 flex-1 flex-col">
                <span className="flex items-center justify-between gap-2">
                  <span className="text-[13px] font-medium text-cx-fg">{item.label}</span>
                  <Kbd>{item.key}</Kbd>
                </span>
                <span className="mt-0.5 text-[12px] leading-[18px] text-cx-fg-3">
                  {unavailable ? "当前会话未绑定工作区" : item.description}
                </span>
              </span>
            </button>
          );
        })}
      </div>
    </div>
  );
}

export interface RightPanelProps extends Omit<SurfaceContext, "active"> {
  /** Render as an overlay sheet (narrow viewports). */
  sheet: boolean;
  sidebarWidth: number;
}

export function RightPanel({ sheet, sidebarWidth, ...ctx }: RightPanelProps) {
  const { threadId, hasWorkspace } = ctx;
  const ownsView = ctx.view.thread.thread_id === threadId;
  const panel = useChatPanel(threadId);
  const width = useChatPanelWidth();
  const [dragging, setDragging] = useState(false);
  const [viewport, setViewport] = useState(1440);
  const dragId = useRef<string | null>(null);
  const bodyRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    const update = () => setViewport(window.innerWidth);
    update();
    window.addEventListener("resize", update);
    return () => window.removeEventListener("resize", update);
  }, []);

  const max = panelWidthMax(viewport, sidebarWidth);
  const effectiveWidth = Math.min(Math.max(width, PANEL_WIDTH_MIN), max);
  const showLauncher = !panel.surfaces.length || !panel.activeId || !panel.surfaces.some((item) => item.id === panel.activeId);

  // Letter shortcuts while the launcher is showing and focus is not in a field.
  useEffect(() => {
    if (!ownsView || !panel.isOpen || !showLauncher) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      const target = event.target as HTMLElement | null;
      if (target?.closest("input, textarea, select, [contenteditable='true'], [data-c34-terminal]")) return;
      const item = LAUNCHER.find((entry) => entry.key.toLowerCase() === event.key.toLowerCase());
      if (!item || (item.needsWorkspace && !hasWorkspace)) return;
      event.preventDefault();
      openLauncherItem(threadId, item);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [panel.isOpen, showLauncher, threadId, hasWorkspace, ownsView]);

  const tabIds = panel.surfaces.map((surface) => surface.id);
  const tablistSelection =
    !showLauncher && panel.activeId && isTabId(tabIds, panel.activeId) ? panel.activeId : tabIds[0];

  const closeSurface = (surfaceId: string) => {
    const before = panel.surfaces;
    const closedIndex = before.findIndex((item) => item.id === surfaceId);
    const remaining = before.filter((item) => item.id !== surfaceId).map((item) => item.id);
    const focusTarget = focusTargetAfterTabClose(
      surfaceId,
      remaining,
      panel.activeId ?? "",
      closedIndex >= 0 ? closedIndex : undefined,
    );
    chatPanel.closeSurface(threadId, surfaceId);
    requestAnimationFrame(() => {
      if (focusTarget.kind === "open-view") {
        document.getElementById(OPEN_VIEW_CONTROL_ID)?.focus();
        return;
      }
      document.getElementById(`${WORKSPACE_TAB_ID_PREFIX}-${focusTarget.tab}`)?.focus();
    });
  };

  const onSurfaceTabListKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement | null;
    if (target?.getAttribute("role") !== "tab" || tabIds.length === 0) return;

    if (event.key === "Delete" || event.key === "Backspace") {
      const raw = target.id.startsWith(`${WORKSPACE_TAB_ID_PREFIX}-`)
        ? target.id.slice(WORKSPACE_TAB_ID_PREFIX.length + 1)
        : "";
      if (raw && isTabId(tabIds, raw)) {
        event.preventDefault();
        closeSurface(raw);
      }
      return;
    }

    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      const raw = target.id.startsWith(`${WORKSPACE_TAB_ID_PREFIX}-`)
        ? target.id.slice(WORKSPACE_TAB_ID_PREFIX.length + 1)
        : "";
      if (raw && isTabId(tabIds, raw) && panel.activeId !== raw) {
        chatPanel.activate(threadId, raw);
      }
      return;
    }

    if (!tablistSelection) return;
    handleTabListKeyDown(
      event,
      tabIds,
      tablistSelection,
      (id) => chatPanel.activate(threadId, id),
      WORKSPACE_TAB_ID_PREFIX,
    );
  };

  const header = (
    <div className="flex h-11 shrink-0 items-center gap-1 border-b border-cx-border-subtle pl-2 pr-1.5">
      <ScrollArea axis="x" className="flex min-w-0 flex-1 items-center">
        <div role="tablist" aria-label="已打开的工作视图" className="flex min-w-max items-center gap-1 py-1" onKeyDown={onSurfaceTabListKeyDown}>
          {panel.surfaces.map((surface) => (
            <SurfaceTab
              key={surface.id}
              threadId={threadId}
              surface={surface}
              active={!showLauncher && surface.id === panel.activeId}
              tabIndex={tablistSelection === surface.id ? 0 : -1}
              onClose={closeSurface}
              onDragStart={(id) => { dragId.current = id; }}
              onDrop={(id) => { if (dragId.current) chatPanel.reorder(threadId, dragId.current, id); dragId.current = null; }}
            />
          ))}
        </div>
      </ScrollArea>
      <div className="flex shrink-0 items-center gap-0.5 pl-1">
        {ownsView ? <AddSurfaceMenu threadId={threadId} hasWorkspace={hasWorkspace} /> : <span role="status" className="text-[12px] text-cx-fg-3">等待当前对话</span>}
        {!sheet ? (
          <IconButton
            icon={panel.maximized ? "minimize" : "maximize"}
            label={panel.maximized ? "还原面板" : "最大化面板"}
            size="sm"
            onClick={() => chatPanel.setMaximized(threadId, !panel.maximized)}
          />
        ) : null}
        <IconButton icon="panelRightClose" label="关闭面板" shortcut="mod+alt+b" size="sm" onClick={() => chatPanel.close(threadId)} />
      </div>
    </div>
  );

  const body = (
    <div ref={bodyRef} className="@container/panel relative min-h-0 flex-1">
      {!ownsView ? <div role="status" className="p-4 text-[13px] text-cx-fg-3">正在等待当前对话的数据；请在加载失败后重试，旧对话的内容和操作已隐藏。</div> : null}
      {ownsView && showLauncher ? <Launcher threadId={threadId} hasWorkspace={hasWorkspace} /> : null}
      {(ownsView ? panel.surfaces : []).map((surface) => {
        const active = !showLauncher && surface.id === panel.activeId;
        return (
          <div
            key={surface.id}
            id={`workspace-surface-panel-${surface.id}`}
            role="tabpanel"
            aria-labelledby={`${WORKSPACE_TAB_ID_PREFIX}-${surface.id}`}
            hidden={!active}
            inert={!active}
            className="absolute inset-0 flex flex-col"
          >
            <SurfaceRenderer surface={surface} ctx={{ ...ctx, active }} />
          </div>
        );
      })}
    </div>
  );

  if (sheet) {
    return (
      <Sheet
        open={panel.isOpen}
        onOpenChange={(open) => { if (!open) chatPanel.close(threadId); }}
        width="min(560px, 92vw)"
        ariaLabel="会话工作面板"
        bodyClassName="flex flex-col overflow-hidden"
        className="cx-root"
      >
        {header}
        {body}
      </Sheet>
    );
  }

  const open = panel.isOpen;
  const maximized = open && panel.maximized;
  return (
    <aside
      aria-label="会话工作面板"
      aria-hidden={!open}
      inert={!open}
      data-open={open}
      data-maximized={maximized || undefined}
      className={cn(
        "flex h-full shrink-0 overflow-hidden border-l bg-cx-bg",
        open ? "border-cx-border-subtle" : "border-transparent",
        !dragging && "transition-[width,left,border-color] duration-[280ms] ease-cx-drawer",
        maximized ? "absolute inset-y-0 right-0 z-30" : "relative",
      )}
      style={maximized ? { left: sidebarWidth } : { width: open ? effectiveWidth : 0 }}
    >
      {open && !maximized ? (
        <ResizeHandle
          side="left"
          value={effectiveWidth}
          min={PANEL_WIDTH_MIN}
          max={max}
          onChange={(next) => chatPanel.setWidth(next)}
          onReset={() => chatPanel.setWidth(PANEL_WIDTH_DEFAULT)}
          onDragStateChange={setDragging}
          label="拖拽调整面板宽度"
          className="left-0"
        />
      ) : null}
      <div className="flex h-full min-w-0 flex-col" style={{ width: maximized ? "100%" : effectiveWidth }}>
        {header}
        {body}
      </div>
    </aside>
  );
}
