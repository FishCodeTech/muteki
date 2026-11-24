"use client";

import Link, { useLinkStatus } from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { createContext, lazy, Suspense, useCallback, useContext, useEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { WorkspaceBoundary } from "@/components/WorkspaceBoundary";
import { ConversationChromeContext, type ConversationChrome } from "@/components/conversationChrome";
import { Icon, Menu, MenuItem, MenuLabel, MenuSeparator } from "@muteki/ui";
import { MotionIcon } from "@/components/MotionIcon";
import { Tooltip } from "@muteki/ui";
import {
  readSavedTheme,
  type ThemeMode,
} from "@/lib/palette-engine";
import { setThemePreference } from "@/lib/themePreference";
import { isMacPlatform, matchesBinding, shortcutBinding } from "@/lib/shortcutBindings";
import { useWorkspaceOverview, type WorkspaceOverview } from "@/lib/workspace-overview";
import { useMediaQuery } from "@/lib/useMediaQuery";
import {
  clampRailWidth,
  CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY,
  CONVERSATION_SIDEBAR_WIDTH_STORAGE_KEY,
  RAIL_WIDTH_DEFAULT,
  RAIL_WIDTH_STORAGE_KEY,
} from "@/lib/railSizing";
import {
  isConversationLayoutPath,
  workspaceFrameContentId,
  workspaceSkipTargetId,
} from "@/lib/workspaceSkipTarget";
import { useSolveOnlyMode } from "@/lib/workspaceMode";
import { isLocalPreviewPath } from "@/lib/conversationPreviewMode";
import { isDesktopRemote } from "@/lib/desktopEnvironment";
import { useActiveRunWorkspace } from "@/lib/activeRunWorkspace";
import { isCtfWorkbenchPath, runIdFromPathname } from "@/lib/runWorkspaceRoutes";
import { useDesktopChrome } from "@/components/DesktopConnection";
import { preloadRetainedWorkspace, RetainedWorkspaces } from "@/components/RetainedWorkspaces";
import { taskManagementGroups } from "@/components/settings/taskManagementLinks";
import { useLang } from "@/lib/i18n";

import { iconOf } from "@/lib/workspaceNavigation";
export { recentStateLabel } from "@/lib/workspaceNavigation";

const CtfWorkspace = lazy(() => import("@/components/RunWorkbench").then((module) => ({ default: module.CtfWorkspace })));

const GlobalWorkspacePalette = lazy(() => import("@/components/GlobalWorkspacePalette").then((module) => ({ default: module.GlobalWorkspacePalette })));

const OverviewContext = createContext<WorkspaceOverview | null>(null);

export function useSharedWorkspaceOverview(): WorkspaceOverview {
  const value = useContext(OverviewContext);
  if (value === null) throw new Error("WorkspaceFrame 缺少 WorkspaceOverview Provider");
  return value;
}

const RAIL_KIND_ORDER = ["/chat", "/ctf", "/pentest", "/src", "/competitions"];
const DESKTOP_RAIL_ENTRIES = [
  { id: "conversation", route: "/chat", title: "对话", aggregateType: "thread", icon: "messages" },
  { id: "single-security-task", route: "/ctf", title: "CTF 工作台", aggregateType: "run", icon: "crosshair" },
  { id: "pentest", route: "/pentest", title: "渗透测试", aggregateType: "run", icon: "target" },
  { id: "src", route: "/src", title: "SRC 赏金", aggregateType: "src", icon: "shield" },
  { id: "competition", route: "/competitions", title: "比赛", aggregateType: "competition", icon: "trophy" },
] as const;
function railKindRank(route: string): number {
  const index = RAIL_KIND_ORDER.indexOf(route);
  return index < 0 ? RAIL_KIND_ORDER.length : index;
}

function RailNavigationPending() {
  const { pending } = useLinkStatus();
  return pending ? <span className="workspace-rail-pending" aria-hidden="true" /> : null;
}

function RailSettingsMenu({ solveOnly }: { solveOnly: boolean }) {
  const router = useRouter();
  const { lang } = useLang();
  const en = lang === "en";
  const title = en ? "Settings" : "设置";
  const general = <MenuItem icon="gear" onSelect={() => router.push("/settings")}>{en ? "Settings center" : "设置中心"}</MenuItem>;
  return (
    <Menu ariaLabel={title} placement="right" trigger={(
      <button type="button" className="workspace-rail-control" data-workspace-action="settings" aria-haspopup="menu" aria-label={en ? "Open settings" : "打开设置"} title={title}>
        <Icon name="gear" size={19} />
      </button>
    )}>
      {!solveOnly && <>{general}<MenuSeparator /></>}
      {taskManagementGroups(en).map(group => (
        <div key={group.id} role="group" aria-label={group.label}>
          <MenuLabel>{group.label}</MenuLabel>
          {group.items.map(item => (
            <MenuItem key={item.id} icon={item.icon} onSelect={() => router.push(item.href)}>{item.label}</MenuItem>
          ))}
        </div>
      ))}
      {solveOnly && <><MenuSeparator />{general}</>}
    </Menu>
  );
}

function WorkspaceNav({ overview, sidebarToggle, onOpenSearch }: {
  overview: WorkspaceOverview;
  sidebarToggle: ReactNode;
  onOpenSearch: () => void;
}) {
  const pathname = usePathname();
  const desktop = useDesktopChrome();
  const solveOnly = useSolveOnlyMode();
  const kinds = overview.kinds.map(kind => ({ ...kind, icon: kind.aggregateType === "thread" ? "messages" as const : iconOf(kind) }));
  const desktopKinds = DESKTOP_RAIL_ENTRIES.map(entry => ({ ...entry, id: kinds.find(kind => kind.route === entry.route)?.id || entry.id }));
  const visibleKinds = (desktop?.native ? [...desktopKinds, ...kinds.filter(kind => !DESKTOP_RAIL_ENTRIES.some(entry => entry.route === kind.route))] : kinds)
    .filter((kind) => desktop?.native || !solveOnly || kind.aggregateType === "run")
    .sort((a, b) => railKindRank(a.route) - railKindRank(b.route));
  const pathRunId = runIdFromPathname(pathname);
  const published = useActiveRunWorkspace();
  const recentKind = pathRunId
    ? overview.recent.find((item) => item.id === pathRunId)?.kindId
    : undefined;
  const onCollaboration = /\/collaboration\/?$/.test(pathname);
  const runWorkspaceMode = pathRunId && published?.runId === pathRunId
    ? published.mode
    : recentKind === "pentest"
      ? "pentest"
      : recentKind
        ? "ctf"
        : pathRunId && !onCollaboration
          ? "ctf"
          : null;
  const [theme, setTheme] = useState<ThemeMode>("dark");

  useEffect(() => {
    setTheme(readSavedTheme());
    const observer = new MutationObserver(() => {
      setTheme(document.documentElement.dataset.theme === "light" ? "light" : "dark");
    });
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    return () => observer.disconnect();
  }, []);

  const toggleTheme = () => {
    const next: ThemeMode = theme === "dark" ? "light" : "dark";
    setTheme(next);
    setThemePreference(next);
  };
  const themeLabel = theme === "dark" ? "切换到亮色模式" : "切换到暗色模式";

  return (
    <aside className="workspace-activity-rail" aria-label="工作台导航">
      <div className="workspace-rail-main">
        {!desktop?.native && <Tooltip content="首页" placement="right">
          <Link href="/" className="workspace-rail-control" aria-label="首页" aria-current={pathname === "/" ? "page" : undefined}>
            <Icon name="home" size={19} />
            <RailNavigationPending />
          </Link>
        </Tooltip>}
        <nav id="workspace-navigation" className="workspace-rail-links" aria-label="工作区">
          {visibleKinds.map((item) => {
            const active = pathname === item.route || pathname.startsWith(`${item.route}/`)
              || (item.route === "/ctf" && (pathname === "/solve" || (pathname.startsWith("/run/") && runWorkspaceMode === "ctf")))
              || (item.route === "/pentest" && pathname.startsWith("/run/") && runWorkspaceMode === "pentest");
            const status = overview.activity[item.id];
            const attention = (status?.unread ?? 0) + (status?.approvals ?? 0);
            return (
              <Tooltip key={item.id} content={item.title} placement="right">
                <Link href={item.route} className="workspace-rail-control" aria-label={item.title} aria-current={active ? "page" : undefined}
                  onPointerEnter={() => preloadRetainedWorkspace(item.route)} onFocus={() => preloadRetainedWorkspace(item.route)}>
                  <Icon name={item.icon} size={19} />
                  <strong className="sr-only">{item.title}</strong>
                  {attention > 0 ? <b className="workspace-rail-badge" data-workspace-badge="" aria-label={`${attention} 项待处理`}>{attention}</b> : null}
                  <RailNavigationPending />
                </Link>
              </Tooltip>
            );
          })}
        </nav>
        {desktop?.native || !solveOnly ? <Tooltip content="全局用量" placement="right">
          <Link href="/usage" className="workspace-rail-control" data-workspace-action="usage" aria-label="全局用量" aria-current={pathname === "/usage" ? "page" : undefined}>
            <Icon name="chartCombined" size={19} />
            <RailNavigationPending />
          </Link>
        </Tooltip> : null}
        <div className="workspace-rail-divider" />
        <Tooltip content="全局搜索" placement="right">
          <button type="button" className="workspace-rail-control" data-workspace-action="search" aria-label="打开全局搜索" onClick={onOpenSearch}><Icon name="search" size={19} /></button>
        </Tooltip>
        {!desktop?.native && sidebarToggle}
      </div>
      <div className="workspace-rail-bottom">
        {desktop?.native && <Tooltip content="刷新当前页面" placement="right">
          <button type="button" className="workspace-rail-control" aria-label="刷新当前页面" onClick={() => window.dispatchEvent(new Event("muteki:reload"))}><Icon name="refresh" size={19} /></button>
        </Tooltip>}
        <Tooltip content={themeLabel} placement="right">
          <button type="button" className="workspace-rail-control" data-workspace-action="theme" aria-label={themeLabel} onClick={toggleTheme}>
            <MotionIcon active={theme === "light"} from="sun" to="moon" size={19} />
          </button>
        </Tooltip>
        <RailSettingsMenu key={pathname} solveOnly={solveOnly} />
        {desktop?.native && <>
          <Tooltip content="服务连接" placement="right"><button type="button" className="workspace-rail-control" aria-label="服务连接" onClick={desktop.configure}><Icon name="plug" size={19} /></button></Tooltip>
          <Tooltip content="桌面端说明" placement="right"><button type="button" className="workspace-rail-control" aria-label="桌面端说明" onClick={desktop.help}><Icon name="help" size={19} /></button></Tooltip>
        </>}
      </div>
    </aside>
  );
}

export function WorkspaceFrame({ children }: { children: ReactNode }) {
  const pathname = usePathname();
  const desktop = useDesktopChrome();
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchLoaded, setSearchLoaded] = useState(false);
  useEffect(() => { if (searchOpen) setSearchLoaded(true); }, [searchOpen]);
  useEffect(() => setSearchOpen(false), [pathname]);
  useEffect(() => {
    const localRunPalette = pathname.startsWith("/run/") || pathname === "/ctf" || pathname === "/pentest";
    if (localRunPalette) return;
    const onKey = (event: KeyboardEvent) => {
      if (matchesBinding(event, shortcutBinding("search"), isMacPlatform())) {
        event.preventDefault();
        setSearchOpen((value) => !value);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [pathname]);
  const compactScreen = useMediaQuery("(max-width: 768px)");
  const solveOnly = useSolveOnlyMode();
  const conversationLayout = isConversationLayoutPath(pathname);
  const detachedRunSideChat = /^\/run\/[^/]+\/side\/[^/]+\/?$/.test(pathname)
    && typeof window !== "undefined" && new URLSearchParams(window.location.search).get("popout") === "1";
  const overview = useWorkspaceOverview(
    conversationLayout ? 60000 : 10000,
    desktop?.native ? false : solveOnly,
    !detachedRunSideChat && !isLocalPreviewPath(pathname) && (!isDesktopRemote() || pathname === "/"),
  );
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [sidebarWidth, setSidebarWidthState] = useState(RAIL_WIDTH_DEFAULT);
  const [sidebarPreferencesReady, setSidebarPreferencesReady] = useState(false);
  const [mobileSidebarOpen, setMobileSidebarOpen] = useState(false);

  useEffect(() => {
    try {
      setSidebarCollapsed(window.localStorage.getItem(CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY) === "1");
      const storedWidth = Number(window.localStorage.getItem(RAIL_WIDTH_STORAGE_KEY) ?? window.localStorage.getItem(CONVERSATION_SIDEBAR_WIDTH_STORAGE_KEY));
      if (Number.isFinite(storedWidth) && storedWidth > 0) {
        setSidebarWidthState(clampRailWidth(storedWidth));
      }
    } catch {
    } finally {
      setSidebarPreferencesReady(true);
    }
  }, []);

  useEffect(() => {
    if (!sidebarPreferencesReady) return;
    try {
      window.localStorage.setItem(RAIL_WIDTH_STORAGE_KEY, String(sidebarWidth));
      window.localStorage.setItem(CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY, sidebarCollapsed ? "1" : "0");
    } catch {
    }
  }, [sidebarCollapsed, sidebarWidth, sidebarPreferencesReady]);

  useEffect(() => {
    setMobileSidebarOpen(false);
  }, [pathname]);

  const toggleSidebarCollapsed = useCallback(() => {
    setSidebarCollapsed((current) => !current);
  }, []);

  const setSidebarWidth = useCallback((width: number) => {
    setSidebarWidthState(clampRailWidth(width, window.innerWidth));
  }, []);

  const headerSlot = desktop?.headerSlot;
  const previewSidebarWidth = useCallback((width: number | null) => {
    if (!headerSlot) return;
    if (width == null) headerSlot.style.removeProperty("--desktop-sidebar-width");
    else headerSlot.style.setProperty("--desktop-sidebar-width", `${width}px`);
  }, [headerSlot]);
  const conversationChrome = useMemo<ConversationChrome>(() => ({
    sidebarCollapsed,
    setSidebarCollapsed,
    toggleSidebarCollapsed,
    sidebarWidth,
    setSidebarWidth,
    compactScreen,
    mobileSidebarOpen,
    setMobileSidebarOpen,
    headerSlot,
    previewSidebarWidth,
  }), [sidebarCollapsed, toggleSidebarCollapsed, sidebarWidth, setSidebarWidth, compactScreen, mobileSidebarOpen, headerSlot, previewSidebarWidth]);

  const sidebarExpanded = compactScreen ? mobileSidebarOpen : !sidebarCollapsed;
  const setDesktopSidebarExpanded = desktop?.setSidebarExpanded;
  useEffect(() => { setDesktopSidebarExpanded?.(sidebarExpanded); }, [setDesktopSidebarExpanded, sidebarExpanded]);
  useEffect(() => {
    if (!desktop?.native) return;
    document.documentElement.style.setProperty("--desktop-sidebar-width", `${conversationLayout && sidebarExpanded ? sidebarWidth : 0}px`);
    return () => { document.documentElement.style.removeProperty("--desktop-sidebar-width"); };
  }, [desktop?.native, conversationLayout, sidebarExpanded, sidebarWidth]);
  const sidebarLabel = sidebarExpanded ? "收起对话导航" : "展开对话导航";
  const toggleConversationSidebar = () => {
    if (compactScreen) setMobileSidebarOpen((open) => !open);
    else toggleSidebarCollapsed();
  };

  const sidebarCommand = useRef<() => void>(() => {});
  sidebarCommand.current = () => { if (conversationLayout) toggleConversationSidebar(); };
  useEffect(() => {
    const onCommand = (event: Event) => {
      if ((event as CustomEvent<{ name?: string }>).detail?.name === "sidebar") sidebarCommand.current();
    };
    window.addEventListener("muteki:command", onCommand);
    return () => window.removeEventListener("muteki:command", onCommand);
  }, []);

  const skipTargetId = workspaceSkipTargetId(pathname);
  const frameContentId = workspaceFrameContentId(pathname);
  const conversationSidebarId = pathname === "/chat" || pathname.startsWith("/chat/")
    ? "chat-conversation-sidebar"
    : pathname === "/src" || pathname.startsWith("/src/tasks/") ? "src-conversation-sidebar" : "conversation-sidebar";

  return (
    <OverviewContext.Provider value={overview}>
      <ConversationChromeContext.Provider value={conversationChrome}>
      <div
        className="workspace-frame"
        data-solve-only={solveOnly ? "true" : undefined}
        data-conversation-layout={conversationLayout ? "true" : undefined}
        data-conversation-sidebar-collapsed={sidebarCollapsed ? "true" : "false"}
        style={{ "--conv-sidebar-width": `${sidebarWidth}px` } as CSSProperties}
      >
        <a className="skip-link" href={`#${skipTargetId}`}>跳到主要内容</a>
        <WorkspaceNav overview={overview} onOpenSearch={() => setSearchOpen(true)} sidebarToggle={conversationLayout ? (
          <Tooltip content={sidebarLabel} placement="right">
            <button type="button" className="workspace-rail-control workspace-rail-sidebar-toggle" data-workspace-action="sidebar"
              aria-label={sidebarLabel} aria-expanded={sidebarExpanded} aria-controls={conversationSidebarId} onClick={toggleConversationSidebar}>
              <Icon name="panelLeft" size={19} />
            </button>
          </Tooltip>
        ) : null} />
        <div id={frameContentId} tabIndex={frameContentId ? -1 : undefined} className="workspace-frame-content" data-workspace-path={pathname}>
          <RetainedWorkspaces />
          <WorkspaceBoundary name={pathname.startsWith("/pentest") ? "渗透" : pathname.startsWith("/ctf") || pathname.startsWith("/competitions") ? "CTF" : "工作区"} resetKey={pathname}>{isCtfWorkbenchPath(pathname) ? (
            <Suspense fallback={null}><CtfWorkspace /></Suspense>
          ) : children}</WorkspaceBoundary>
        </div>
        {searchOpen || searchLoaded ? <Suspense fallback={null}>
          <GlobalWorkspacePalette overview={overview} open={searchOpen} setOpen={setSearchOpen} />
        </Suspense> : null}
      </div>
      </ConversationChromeContext.Provider>
    </OverviewContext.Provider>
  );
}
