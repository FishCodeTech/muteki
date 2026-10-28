"use client";

import {
  memo,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import { REFLOW_PANEL_ATTR } from "@/components/ai-native/chat";
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
  Shortcut,
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
import {
  PanelSheetContext,
  extensionPanelRatio,
  extensionSurface,
  findPanelExtension,
  useExtensionPanelRatio,
  usePanelExtensions,
  type PanelExtensionConfig,
} from "./extensions";

/** One launcher row: a built-in surface or a host extension. */
interface LauncherEntry {
  id: string;
  label: string;
  description: string;
  icon: IconName;
  /** Single letter that opens it while the launcher shows. */
  key?: string;
  shortcut?: string;
  disabled: boolean;
  open: (threadId: string) => void;
}

function launcherEntries(config: PanelExtensionConfig | null, hasWorkspace: boolean): LauncherEntry[] {
  const builtIn: LauncherEntry[] = config?.exclusive ? [] : LAUNCHER.map((item) => ({
    id: item.kind,
    label: item.label,
    description: item.needsWorkspace && !hasWorkspace ? "当前会话未绑定工作区" : item.description,
    icon: item.icon,
    key: item.key,
    disabled: Boolean(item.needsWorkspace && !hasWorkspace),
    open: (threadId: string) => openLauncherItem(threadId, item),
  }));
  const extensions: LauncherEntry[] = (config?.extensions ?? []).map((item) => ({
    id: `ext:${item.key}`,
    label: item.label,
    description: item.description ?? "",
    icon: item.icon,
    shortcut: item.shortcut,
    disabled: false,
    open: (threadId: string) => chatPanel.open(threadId, extensionSurface(item.key, item.label)),
  }));
  return [...extensions, ...builtIn];
}

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
  { kind: "agents", label: "子代理", description: "子代理的运行状态、层级与操作", icon: "bot", key: "A" },
  { kind: "pull-request", label: "Pull request", description: "当前分支关联的 Pull request", icon: "gitPullRequest", key: "R", needsWorkspace: true },
];

function openLauncherItem(threadId: string, item: LauncherItem) {
  if (item.kind === "preview") chatPanel.openPreview(threadId, null, { newTab: true });
  else chatPanel.open(threadId, item.kind);
}

function surfaceIcon(surface: ChatSurface, config: PanelExtensionConfig | null): IconName {
  if (surface.kind === "file") return "fileCode";
  if (surface.kind === "extension") return findPanelExtension(config, surface.key)?.icon ?? (surfaceMeta("extension").icon as IconName);
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
const SurfaceTab = memo(function SurfaceTab({
  domId,
  threadId,
  surface,
  active,
  tabIndex,
  onClose,
  onDragStart,
  onDrop,
}: {
  domId: string;
  threadId: string;
  surface: ChatSurface;
  active: boolean;
  tabIndex: number;
  onClose: (id: string) => void;
  onDragStart: (id: string) => void;
  onDrop: (id: string) => void;
}) {
  const ctx = useContextMenu();
  const extensions = usePanelExtensions();
  const title = surface.kind === "extension"
    ? findPanelExtension(extensions, surface.key)?.label ?? surfaceTitle(surface)
    : surfaceTitle(surface);
  const ref = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (active) ref.current?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [active, surface.id, threadId]);
  return (
    <>
      <div
        ref={ref}
        id={`${domId}-tab-${surface.id}`}
        role="tab"
        aria-selected={active}
        aria-controls={`${domId}-panel-${surface.id}`}
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
          "group/tab relative flex h-7 max-w-[180px] shrink-0 cursor-default select-none items-center gap-1.5 rounded-lg pl-2 pr-1 text-[13px] font-medium outline-none transition-colors duration-150",
          // Extension hosts get 32px targets; the rem-based h-7 renders smaller under the app's 14px root font size.
          extensions && "h-[32px]",
          active
            ? "bg-cx-elevated text-cx-fg shadow-[0_0_0_1px_var(--cx-border),0_1px_2px_hsl(var(--cx-shadow-color)/0.06)]"
            : "text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg",
          "focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
        )}
      >
        <span className={cn("grid size-4 shrink-0 place-items-center", active ? "text-cx-fg-2" : "text-cx-fg-4")}>
          {surface.kind === "preview" ? <Favicon url={surface.url} /> : <Icon name={surfaceIcon(surface, extensions)} size={13} />}
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
});

function EntryHint({ entry, tone }: { entry: LauncherEntry; tone?: "subtle" }) {
  if (entry.key) return <Kbd tone={tone}>{entry.key}</Kbd>;
  if (entry.shortcut) return <Shortcut keys={entry.shortcut} tone={tone} />;
  return null;
}

function AddSurfaceMenu({ threadId, entries, className, openViewId }: { threadId: string; entries: LauncherEntry[]; className?: string; openViewId: string }) {
  return (
    <Menu
      placement="bottom-start"
      ariaLabel="打开工作视图"
      trigger={<IconButton id={openViewId} icon="plus" label="打开视图" size="xs" className={cn("ml-0.5", className)} />}
    >
      <MenuLabel>打开视图</MenuLabel>
      {entries.map((entry) => (
        <MenuItem
          key={entry.id}
          icon={entry.icon}
          disabled={entry.disabled}
          hint={<EntryHint entry={entry} tone="subtle" />}
          onSelect={() => entry.open(threadId)}
        >
          {entry.label}
        </MenuItem>
      ))}
    </Menu>
  );
}

function Launcher({ threadId, entries, title, description }: { threadId: string; entries: LauncherEntry[]; title?: string; description?: string }) {
  return (
    <div className="cx-scroll flex h-full flex-col overflow-y-auto px-5 pb-6 pt-8">
      <div className="mb-5">
        <h2 className="text-[15px] font-semibold tracking-[-0.01em] text-cx-fg">{title ?? "打开工作视图"}</h2>
        <p className="mt-1 text-[13px] text-cx-fg-3">{description ?? "在右侧面板并排查看变更、预览、文件和终端。按字母键快速打开。"}</p>
      </div>
      <div className="grid grid-cols-1 gap-1.5 @[440px]/panel:grid-cols-2">
        {entries.map((entry) => (
          <button
            key={entry.id}
            type="button"
            disabled={entry.disabled}
            onClick={() => entry.open(threadId)}
            className="cx-press group flex items-start gap-3 rounded-xl border border-transparent p-3 text-left transition-colors hover:border-cx-border hover:bg-cx-elevated hover:shadow-cx-xs disabled:cursor-not-allowed disabled:opacity-45"
          >
            <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-cx-hover text-cx-fg-2 transition-colors group-hover:bg-cx-accent-soft group-hover:text-cx-accent">
              <Icon name={entry.icon} size={16} />
            </span>
            <span className="flex min-w-0 flex-1 flex-col">
              <span className="flex items-center justify-between gap-2">
                <span className="text-[13px] font-medium text-cx-fg">{entry.label}</span>
                <EntryHint entry={entry} />
              </span>
              <span className="mt-0.5 text-[12px] leading-[18px] text-cx-fg-3">{entry.description}</span>
            </span>
          </button>
        ))}
      </div>
    </div>
  );
}

export interface RightPanelProps extends Omit<SurfaceContext, "active"> {
  /** Render as an overlay sheet (narrow viewports). */
  sheet: boolean;
  sidebarWidth: number;
}

/**
 * Returns a function with stable identity that always calls the latest `fn`, or
 * undefined when `fn` is absent (surfaces treat a missing callback as "feature off").
 */
function useLatestCallback<A extends unknown[], R>(fn: ((...args: A) => R) | undefined): ((...args: A) => R) | undefined {
  const ref = useRef(fn);
  useLayoutEffect(() => {
    ref.current = fn;
  });
  const stable = useCallback((...args: A) => ref.current!(...args), []);
  return fn ? stable : undefined;
}

export function RightPanel({
  sheet,
  sidebarWidth,
  threadId,
  view,
  events,
  tools,
  hasWorkspace,
  onOpenDetails,
  onCiteToComposer,
  onAttachToComposer,
  onDiffAnnotationSend,
}: RightPanelProps) {
  const ownsView = view.thread.thread_id === threadId;
  // The same thread can render inside two retained shells; ids must be unique per panel.
  const domId = `workspace-surface${useId()}`;
  const extensions = usePanelExtensions();
  const entries = useMemo(() => launcherEntries(extensions, hasWorkspace), [extensions, hasWorkspace]);
  const panel = useChatPanel(threadId);
  const storedWidth = useChatPanelWidth();
  const ratioConfig = extensions?.widthRatio;
  const storedRatio = useExtensionPanelRatio();
  const [dragging, setDragging] = useState(false);
  const [viewport, setViewport] = useState(1440);
  const dragId = useRef<string | null>(null);
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const asideRef = useRef<HTMLElement | null>(null);
  const columnRef = useRef<HTMLDivElement | null>(null);
  const draftWidth = useRef<number | null>(null);
  const draftFrame = useRef<number | null>(null);

  const stableOpenDetails = useLatestCallback(onOpenDetails)!;
  const stableCite = useLatestCallback(onCiteToComposer);
  const stableAttach = useLatestCallback(onAttachToComposer);
  const stableDiffSend = useLatestCallback(onDiffAnnotationSend);
  const baseCtx = useMemo<Omit<SurfaceContext, "active">>(
    () => ({
      threadId,
      view,
      events,
      tools,
      hasWorkspace,
      onOpenDetails: stableOpenDetails,
      onCiteToComposer: stableCite,
      onAttachToComposer: stableAttach,
      onDiffAnnotationSend: stableDiffSend,
    }),
    [threadId, view, events, tools, hasWorkspace, stableOpenDetails, stableCite, stableAttach, stableDiffSend],
  );
  const activeCtx = useMemo<SurfaceContext>(() => ({ ...baseCtx, active: true }), [baseCtx]);
  const inactiveCtx = useMemo<SurfaceContext>(() => ({ ...baseCtx, active: false }), [baseCtx]);

  useEffect(() => () => {
    if (draftFrame.current != null) cancelAnimationFrame(draftFrame.current);
  }, []);

  // While dragging, widths are written straight to the DOM once per frame and
  // committed to the store on release, so the shell is not re-rendered per pointermove.
  // A ref (not `dragging` state) is required: ResizeHandle's move listener keeps the
  // onChange from the render in which the drag started.
  const draggingRef = useRef(false);
  const applyDraftWidth = () => {
    draftFrame.current = null;
    const next = draftWidth.current;
    if (next == null) return;
    if (asideRef.current) asideRef.current.style.width = `${next}px`;
    if (columnRef.current) columnRef.current.style.width = `${next}px`;
  };
  const commitWidth = (next: number) => {
    if (ratioConfig) extensionPanelRatio.set(next / Math.max(1, viewport - sidebarWidth));
    else chatPanel.setWidth(next);
  };
  const onResizeChange = (next: number) => {
    if (!draggingRef.current) {
      commitWidth(next);
      return;
    }
    draftWidth.current = next;
    if (draftFrame.current == null) draftFrame.current = requestAnimationFrame(applyDraftWidth);
  };
  const onResizeDragStateChange = (active: boolean) => {
    draggingRef.current = active;
    if (!active) {
      if (draftFrame.current != null) cancelAnimationFrame(draftFrame.current);
      applyDraftWidth();
      const next = draftWidth.current;
      draftWidth.current = null;
      if (next != null) commitWidth(next);
    }
    setDragging(active);
  };

  useEffect(() => {
    const update = () => setViewport(window.innerWidth);
    update();
    window.addEventListener("resize", update);
    return () => window.removeEventListener("resize", update);
  }, []);

  const available = Math.max(0, viewport - sidebarWidth);
  const min = ratioConfig ? ratioConfig.panelMin : PANEL_WIDTH_MIN;
  const max = ratioConfig ? Math.max(min, available - ratioConfig.mainMin) : panelWidthMax(viewport, sidebarWidth);
  const width = ratioConfig ? Math.round(available * (storedRatio ?? ratioConfig.initial)) : storedWidth;
  const effectiveWidth = Math.min(Math.max(width, min), max);
  const docked = extensions?.focusLayout === "dock";
  const showLauncher = !panel.surfaces.length || !panel.activeId || !panel.surfaces.some((item) => item.id === panel.activeId);

  // Letter shortcuts while the launcher is showing and focus is not in a field.
  useEffect(() => {
    if (!ownsView || !panel.isOpen || !showLauncher) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      const target = event.target as HTMLElement | null;
      if (target?.closest("input, textarea, select, [contenteditable='true'], [data-c34-terminal]")) return;
      const entry = entries.find((item) => item.key && item.key.toLowerCase() === event.key.toLowerCase());
      if (!entry || entry.disabled) return;
      event.preventDefault();
      entry.open(threadId);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [panel.isOpen, showLauncher, threadId, entries, ownsView]);

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
        document.getElementById(`${domId}-open-view`)?.focus();
        return;
      }
      document.getElementById(`${domId}-tab-${focusTarget.tab}`)?.focus();
    });
  };
  const stableCloseSurface = useLatestCallback(closeSurface)!;
  const onTabDragStart = useCallback((id: string) => { dragId.current = id; }, []);
  const onTabDrop = useCallback((id: string) => {
    if (dragId.current) chatPanel.reorder(threadId, dragId.current, id);
    dragId.current = null;
  }, [threadId]);

  const onSurfaceTabListKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement | null;
    if (target?.getAttribute("role") !== "tab" || tabIds.length === 0) return;

    if (event.key === "Delete" || event.key === "Backspace") {
      const raw = target.id.startsWith(`${domId}-tab-`)
        ? target.id.slice(domId.length + 5)
        : "";
      if (raw && isTabId(tabIds, raw)) {
        event.preventDefault();
        closeSurface(raw);
      }
      return;
    }

    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      const raw = target.id.startsWith(`${domId}-tab-`)
        ? target.id.slice(domId.length + 5)
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
      `${domId}-tab`,
    );
  };

  // Only extension hosts (not the regular chat) get 32px header controls.
  const control = extensions ? "h-[32px] w-[32px]" : undefined;
  const header = (
    <div className={cn("flex h-11 shrink-0 items-center gap-1 border-b border-cx-border-subtle pl-2 pr-1.5", extensions && "h-[44px]")}>
      <ScrollArea axis="x" className="flex min-w-0 flex-1 items-center">
        <div role="tablist" aria-label="已打开的工作视图" className="flex min-w-max items-center gap-1 py-1" onKeyDown={onSurfaceTabListKeyDown}>
          {panel.surfaces.map((surface) => (
            <SurfaceTab
              key={surface.id}
              domId={domId}
              threadId={threadId}
              surface={surface}
              active={!showLauncher && surface.id === panel.activeId}
              tabIndex={tablistSelection === surface.id ? 0 : -1}
              onClose={stableCloseSurface}
              onDragStart={onTabDragStart}
              onDrop={onTabDrop}
            />
          ))}
        </div>
      </ScrollArea>
      <div className="flex shrink-0 items-center gap-0.5 pl-1">
        {ownsView ? <AddSurfaceMenu threadId={threadId} entries={entries} className={control} openViewId={`${domId}-open-view`} /> : <span role="status" className="text-[12px] text-cx-fg-3">等待当前对话</span>}
        {!sheet ? (
          <IconButton
            icon={panel.maximized ? "minimize" : "maximize"}
            label={docked
              ? (panel.maximized ? "并排显示对话与工作区" : "专注工作区（对话收为底部输入栏）")
              : (panel.maximized ? "还原面板" : "最大化面板")}
            size={docked ? "md" : "sm"}
            className={control}
            onClick={() => chatPanel.setMaximized(threadId, !panel.maximized)}
          />
        ) : null}
        <IconButton icon="panelRightClose" label="关闭面板" shortcut="mod+alt+b" size={docked ? "md" : "sm"} className={control} onClick={() => chatPanel.close(threadId)} />
      </div>
    </div>
  );

  const body = (
    <PanelSheetContext.Provider value={sheet}>
    <div ref={bodyRef} className="@container/panel relative min-h-0 flex-1">
      {!ownsView ? <div role="status" className="p-4 text-[13px] text-cx-fg-3">正在等待当前对话的数据；请在加载失败后重试，旧对话的内容和操作已隐藏。</div> : null}
      {ownsView && showLauncher ? (
        <Launcher threadId={threadId} entries={entries} title={extensions?.launcherTitle} description={extensions?.launcherDescription} />
      ) : null}
      {(ownsView ? panel.surfaces : []).map((surface) => {
        const active = !showLauncher && surface.id === panel.activeId;
        return (
          <div
            key={surface.id}
            id={`${domId}-panel-${surface.id}`}
            role="tabpanel"
            aria-labelledby={`${domId}-tab-${surface.id}`}
            hidden={!active}
            inert={!active}
            className="absolute inset-0 flex flex-col"
          >
            <SurfaceRenderer surface={surface} ctx={active ? activeCtx : inactiveCtx} />
          </div>
        );
      })}
    </div>
    </PanelSheetContext.Provider>
  );

  if (sheet) {
    return (
      <Sheet
        open={panel.isOpen}
        onOpenChange={(open) => { if (!open) chatPanel.close(threadId); }}
        width={typeof extensions?.sheetWidth === "function" ? extensions.sheetWidth(viewport) : extensions?.sheetWidth ?? "min(560px, 92vw)"}
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
  // Docked focus: the shell publishes the composer's height as
  // --cx-panel-dock-inset, so the panel ends where the docked composer starts.
  const dockedFocus = maximized && docked;
  const shownWidth = effectiveWidth;
  return (
    <aside
      ref={asideRef}
      aria-label="会话工作面板"
      aria-hidden={!open}
      inert={!open}
      data-open={open}
      data-maximized={maximized || undefined}
      data-docked={dockedFocus || undefined}
      {...{ [REFLOW_PANEL_ATTR]: "" }}
      className={cn(
        "flex shrink-0 overflow-hidden border-l bg-cx-bg",
        open ? "border-cx-border-subtle" : "border-transparent",
        !dragging && "transition-[width,left,bottom,border-color] duration-[280ms] ease-cx-drawer",
        dockedFocus
          ? "absolute right-0 top-0 z-30 border-b border-l-0 shadow-cx-md"
          : maximized ? "absolute inset-y-0 right-0 z-30 h-full" : "relative h-full",
      )}
      style={dockedFocus
        ? { left: sidebarWidth, bottom: "var(--cx-panel-dock-inset, 0px)" }
        : maximized ? { left: sidebarWidth } : { width: open ? shownWidth : 0 }}
    >
      {open && !maximized ? (
        <ResizeHandle
          side="left"
          value={effectiveWidth}
          min={min}
          max={max}
          onChange={onResizeChange}
          onReset={() => (ratioConfig ? extensionPanelRatio.set(null) : chatPanel.setWidth(PANEL_WIDTH_DEFAULT))}
          onDragStateChange={onResizeDragStateChange}
          label="拖拽调整面板宽度"
          className="left-0"
        />
      ) : null}
      {/* The column keeps a fixed width while the aside animates, so containment keeps
          the outer width transition from relaying out the surface subtree. */}
      <div
        ref={columnRef}
        className="flex h-full min-w-0 flex-col [contain:layout_paint]"
        style={{ width: maximized ? "100%" : shownWidth }}
      >
        {header}
        {body}
      </div>
    </aside>
  );
}
