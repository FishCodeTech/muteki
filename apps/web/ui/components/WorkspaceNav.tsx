"use client";

import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { Button, Input, ListBox, ListBoxItem, Modal } from "@heroui/react";
import { ConversationChromeContext, type ConversationChrome } from "@/components/conversationChrome";
import { Icon, type IconName } from "@/components/Icon";
import { MotionIcon } from "@/components/MotionIcon";
import { MutekiLogo } from "@/components/MutekiLogo";
import { Tooltip } from "@/components/chat/ui/Tooltip";
import { ChartNoAxesCombined, Home } from "lucide-react";
import {
  applySelection,
  readSavedSelection,
  readSavedTheme,
  type ThemeMode,
} from "@/lib/palette-engine";
import { useWorkspaceOverview, type WorkspaceOverview } from "@/lib/workspace-overview";
import type { WorkspaceKindEntry } from "@/lib/workspace-kinds";
import {
  fetchConversationSearch,
  type ConversationSearchHit,
  queryLooksSearchable,
} from "@/lib/useConversation";
import { buildThreadMessageHref } from "@/lib/conversationDeepLink";
import { useMediaQuery } from "@/lib/useMediaQuery";
import {
  clampRailWidth,
  CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY,
  CONVERSATION_SIDEBAR_WIDTH_STORAGE_KEY,
  RAIL_WIDTH_DEFAULT,
} from "@/lib/railSizing";
import {
  workspaceFrameContentId,
  workspaceSkipTargetId,
} from "@/lib/workspaceSkipTarget";
import { useSolveOnlyMode } from "@/lib/workspaceMode";

const OverviewContext = createContext<WorkspaceOverview | null>(null);

export function useSharedWorkspaceOverview(): WorkspaceOverview {
  const value = useContext(OverviewContext);
  if (value === null) throw new Error("WorkspaceFrame 缺少 WorkspaceOverview Provider");
  return value;
}

function iconOf(entry: WorkspaceKindEntry): IconName {
  if (entry.id === "pentest") return "target";
  if (entry.icon === "flag") return "crosshair";
  if (entry.icon === "trophy") return "trophy";
  if (entry.icon === "chat") return "messages";
  return "layers";
}

function publicDescriptionOf(entry: WorkspaceKindEntry): string {
  if (entry.id === "pentest") return "用自然语言启动授权测试并查看证据报告";
  if (entry.aggregateType === "thread") return "与外部 Agent 协作并查看工具、审批和产物";
  if (entry.aggregateType === "run") return "创建 CTF 单题并交给 Coordinator 调度";
  if (entry.aggregateType === "competition") return "同步比赛题目、调度 Run 并跟踪提交裁定";
  return entry.description;
}

export function recentStateLabel(state: string, kindId?: string, running?: boolean): string {
  if (kindId === "conversation" && state === "active" && !running) return "可继续";
  const labels: Record<string, string> = {
    active: "进行中",
    running: "运行中",
    archived: "已归档",
    completed: "已完成",
    failed: "失败",
    paused: "已暂停",
    finished: "已完成",
    stopped: "已停止",
    cancelled: "已取消",
    queued: "排队中",
    pending: "等待中",
    idle: "空闲",
  };
  return labels[state] || state || "可恢复";
}

function GlobalWorkspacePalette({ overview }: { overview: WorkspaceOverview }) {
  const pathname = usePathname();
  const router = useRouter();
  const solveOnly = useSolveOnlyMode();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [bodyHits, setBodyHits] = useState<ConversationSearchHit[]>([]);
  const [bodyHitsLoading, setBodyHitsLoading] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const localRunPalette = pathname.startsWith("/run/") || pathname === "/ctf" || pathname === "/pentest";
  const onChat = pathname.startsWith("/chat");

  useEffect(() => setOpen(false), [pathname]);

  const commands = useMemo(() => {
    const base: Array<{ id: string; label: string; detail: string; href: string; icon: IconName }> = overview.kinds.filter((kind) => !solveOnly || kind.aggregateType === "run").map((kind) => ({
      id: `workspace-${kind.id}`,
      label: `打开${kind.title}`,
      detail: publicDescriptionOf(kind),
      href: kind.route,
      icon: iconOf(kind),
    }));
    if (solveOnly) {
      base.push(
        { id: "task-workers", label: "CTF Worker 配置", detail: "出战池、运行环境与调度预算", href: "/ctf/workers", icon: "cpu" },
        { id: "task-credentials", label: "Agent 凭据", detail: "配置做题 Worker 使用的凭据", href: "/ctf/workers?section=credentials", icon: "lock" },
        { id: "settings-appearance", label: "显示模式", detail: "开启对话与比赛模式", href: "/settings/appearance", icon: "gear" },
      );
    } else base.push(
      { id: "usage", label: "全局用量", detail: "对话、任务与比赛 Token 统计", href: "/usage", icon: "rows" },
      { id: "settings-hub", label: "打开设置", detail: "设置中心：Agents、能力、运维与扩展", href: "/settings/agents", icon: "gear" },
      { id: "settings-agents", label: "Agents", detail: "九类引擎的登录、模型和接入", href: "/settings/agents", icon: "plug" },
      { id: "settings-capabilities", label: "能力管理", detail: "Conversation Thread 授权与全局 MCP/Skills，不是 Fact 图白名单", href: "/settings/capabilities", icon: "network" },
      { id: "task-workers", label: "CTF Worker 配置", detail: "出战池、运行环境、调度预算与推理模型", href: "/ctf/workers", icon: "cpu" },
      { id: "settings-appearance", label: "外观配色", detail: "主题、配色引擎与界面语言", href: "/settings/appearance", icon: "droplet" },
      { id: "settings-extensions", label: "扩展设置", detail: "安装、升级与回滚", href: "/settings/extensions", icon: "layers" },
      { id: "competition-credentials", label: "比赛平台凭据", detail: "连接、轮换、撤销与浏览器会话", href: "/competitions?focus=credentials", icon: "lock" },
      { id: "settings-operations", label: "运行诊断与维护", detail: "指标、回执、恢复与清理", href: "/settings/operations", icon: "radio" },
    );
    for (const item of overview.recent.filter((item) => !solveOnly || item.kindId === "single-security-task").slice(0, 8)) {
      base.push({
        id: `recent-${item.kindId}-${item.id}`,
        label: item.title,
        detail: `最近工作区 · ${recentStateLabel(item.status, item.kindId, item.running)}`,
        href: item.href,
        icon: item.kindId === "conversation" ? "terminal" : item.kindId === "competition" ? "grid" : "target",
      });
    }
    return base;
  }, [overview.kinds, overview.recent, solveOnly]);

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return commands.filter((command) =>
      !needle || `${command.label} ${command.detail}`.toLowerCase().includes(needle),
    );
  }, [commands, query]);
  const bodyHitsReady = !bodyHitsLoading && bodyHits.length > 0;

  useEffect(() => {
    if (!open || !onChat) {
      setBodyHits([]);
      setBodyHitsLoading(false);
      return;
    }
    if (!queryLooksSearchable(query)) {
      setBodyHits([]);
      setBodyHitsLoading(false);
      return;
    }
    const controller = new AbortController();
    setBodyHitsLoading(true);
    const timer = window.setTimeout(() => {
      void fetchConversationSearch(query, {
        limit: 8,
        signal: controller.signal,
      }).then((result) => {
        if (controller.signal.aborted) return;
        setBodyHits(result.hits || []);
        setBodyHitsLoading(false);
      }).catch(() => {
        if (controller.signal.aborted) return;
        setBodyHits([]);
        setBodyHitsLoading(false);
      });
    }, 250);
    return () => {
      window.clearTimeout(timer);
      controller.abort();
    };
  }, [open, onChat, query]);

  useEffect(() => {
    if (localRunPalette) return;
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setOpen((value) => !value);
        return;
      }
      if (open && (event.metaKey || event.ctrlKey) && /^[1-9]$/.test(event.key)) {
        const index = Number(event.key) - 1;
        if (bodyHitsReady && index < bodyHits.length) {
          const hit = bodyHits[index];
          if (!hit) return;
          event.preventDefault();
          setOpen(false);
          router.push(buildThreadMessageHref(hit.thread_id, hit.message_id));
          return;
        }
        const command = filtered[index];
        if (!command) return;
        event.preventDefault();
        setOpen(false);
        router.push(command.href);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [bodyHits, bodyHitsReady, filtered, localRunPalette, open, router]);

  useEffect(() => {
    if (!open) return;
    const previousFocus = document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
    setQuery("");
    const frame = window.requestAnimationFrame(() => inputRef.current?.focus());
    return () => {
      window.cancelAnimationFrame(frame);
      previousFocus?.focus();
    };
  }, [open]);

  if (localRunPalette) return null;
  const choose = (id: string) => {
    if (id.startsWith("conv-hit:")) {
      const hit = bodyHits.find((item) => `conv-hit:${item.thread_id}:${item.message_id}` === id);
      if (!hit) return;
      setOpen(false);
      router.push(buildThreadMessageHref(hit.thread_id, hit.message_id));
      return;
    }
    const command = filtered.find((item) => item.id === id);
    if (!command) return;
    setOpen(false);
    router.push(command.href);
  };

  return (
    <Modal isOpen={open} onOpenChange={setOpen}>
      <Modal.Backdrop className="global-search-backdrop">
      <Modal.Container size="lg" placement="top">
      <Modal.Dialog
        className="global-search-dialog p-0"
        aria-label="搜索工作区"
      >
        <div>
        <Modal.Header><Modal.Heading>搜索工作区</Modal.Heading></Modal.Header>
        <Modal.Body>
        <div className="global-search-field">
          <Icon name="search" size={17} />
          <Input ref={inputRef} value={query} onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.nativeEvent.isComposing || event.metaKey || event.ctrlKey || event.altKey) return;
              if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                const options = event.currentTarget.closest(".global-search-dialog")?.querySelectorAll<HTMLElement>('[role="option"]');
                const option = event.key === "ArrowUp" ? options?.[options.length - 1] : options?.[0];
                if (option) { event.preventDefault(); option.focus(); }
              } else if (event.key === "Enter") {
                const hit = bodyHitsReady ? bodyHits[0] : undefined;
                const id = hit ? `conv-hit:${hit.thread_id}:${hit.message_id}` : filtered[0]?.id;
                if (id) { event.preventDefault(); choose(id); }
              }
            }}
            placeholder={onChat ? "搜索工作区、设置、最近任务或对话正文" : "搜索工作区、设置或最近任务"} aria-label="搜索工作区、设置或最近任务" />
          {query ? <Button size="sm" variant="ghost" isIconOnly onPress={() => setQuery("")} aria-label="清除全局搜索"><Icon name="x" size={14} /></Button> : <kbd>ESC</kbd>}
        </div>
        {onChat ? (
          <>
            <div className="global-search-section-label">对话正文</div>
            {bodyHitsLoading ? <div className="global-search-empty">正在搜索正文…</div> : null}
            {!bodyHitsLoading && bodyHits.length ? (
              <ListBox className="global-search-list" aria-label="对话正文命中" onAction={(key) => choose(String(key))}>
                {bodyHits.map((hit, index) => (
                  <ListBoxItem
                    id={`conv-hit:${hit.thread_id}:${hit.message_id}`}
                    textValue={`${hit.thread_title} ${hit.snippet}`}
                    key={`conv-hit:${hit.thread_id}:${hit.message_id}`}
                  >
                    <span className="global-search-icon"><Icon name="messages" size={16} /></span>
                    <span className="global-search-result-body">
                      <strong>{hit.thread_title || "未命名对话"}</strong>
                      <small>{hit.snippet.replaceAll("«", "").replaceAll("»", "")}</small>
                    </span>
                    {index < 9 ? <kbd>⌘{index + 1}</kbd> : null}
                  </ListBoxItem>
                ))}
              </ListBox>
            ) : null}
            {!bodyHitsLoading && queryLooksSearchable(query) && !bodyHits.length ? (
              <div className="global-search-empty">没有匹配的正文命中</div>
            ) : null}
          </>
        ) : null}
        <div className="global-search-section-label">工作区、设置与最近任务</div>
        {filtered.length ? <ListBox className="global-search-list" aria-label="工作区、设置与最近任务" onAction={(key) => choose(String(key))}>
          {filtered.length ? filtered.map((command, index) => (
            <ListBoxItem id={command.id} textValue={`${command.label} ${command.detail}`} key={command.id}>
              <span className="global-search-icon"><Icon name={command.icon} size={16} /></span>
              <span className="global-search-result-body"><strong>{command.label}</strong><small>{command.detail}</small></span>
              {index < 9 && !bodyHitsReady ? <kbd>⌘{index + 1}</kbd> : null}
            </ListBoxItem>
          )) : <div className="global-search-empty">没有匹配的工作区、设置或最近任务</div>}
        </ListBox> : <div className="global-search-empty">没有匹配的工作区、设置或最近任务</div>}
        </Modal.Body>
        </div>
      </Modal.Dialog>
      </Modal.Container>
      </Modal.Backdrop>
    </Modal>
  );
}

function WorkspaceNav({ overview, sidebarToggle }: {
  overview: WorkspaceOverview;
  sidebarToggle: ReactNode;
}) {
  const pathname = usePathname();
  const solveOnly = useSolveOnlyMode();
  const visibleKinds = overview.kinds.filter((kind) => !solveOnly || kind.aggregateType === "run");
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
    try {
      window.localStorage.setItem("muteki.theme", next);
    } catch {
      /* session-only theming */
    }
    applySelection(readSavedSelection(), next);
  };
  const themeLabel = theme === "dark" ? "切换到亮色模式" : "切换到暗色模式";

  return (
    <aside className="workspace-activity-rail" aria-label="工作台导航">
      <div className="workspace-rail-main">
        <Tooltip content="首页" placement="right">
          <Link href="/" className="workspace-rail-control" aria-label="首页" aria-current={pathname === "/" ? "page" : undefined}>
            <Home size={19} aria-hidden="true" />
          </Link>
        </Tooltip>
        <nav id="workspace-navigation" className="workspace-rail-links" aria-label="工作区">
          {visibleKinds.map((item) => {
            const active = pathname === item.route || pathname.startsWith(`${item.route}/`)
              || (item.route === "/ctf" && (pathname.startsWith("/run/") || pathname === "/solve"));
            const status = overview.activity[item.id];
            const attention = (status?.unread ?? 0) + (status?.approvals ?? 0);
            return (
              <Tooltip key={item.id} content={item.title} placement="right">
                <Link href={item.route} className="workspace-rail-control" aria-label={item.title} aria-current={active ? "page" : undefined}>
                  <Icon name={iconOf(item)} size={19} />
                  <strong className="sr-only">{item.title}</strong>
                  {attention > 0 ? <b className="workspace-rail-badge" data-workspace-badge="" aria-label={`${attention} 项待处理`}>{attention}</b> : null}
                </Link>
              </Tooltip>
            );
          })}
        </nav>
        {!solveOnly ? <Tooltip content="全局用量" placement="right">
          <Link href="/usage" className="workspace-rail-control" data-workspace-action="usage" aria-label="全局用量" aria-current={pathname === "/usage" ? "page" : undefined}>
            <ChartNoAxesCombined size={19} aria-hidden="true" />
          </Link>
        </Tooltip> : null}
        <div className="workspace-rail-divider" />
        <Tooltip content="全局搜索" shortcut={["⌘", "K"]} placement="right">
          <button type="button" className="workspace-rail-control" data-workspace-action="search" aria-label="打开全局搜索" onClick={() => {
            window.dispatchEvent(new KeyboardEvent("keydown", { key: "k", ctrlKey: true }));
          }}><Icon name="search" size={19} /></button>
        </Tooltip>
        {sidebarToggle}
      </div>
      <div className="workspace-rail-bottom">
        <Tooltip content={themeLabel} placement="right">
          <button type="button" className="workspace-rail-control" data-workspace-action="theme" aria-label={themeLabel} onClick={toggleTheme}>
            <MotionIcon active={theme === "light"} from="sun" to="moon" size={19} />
          </button>
        </Tooltip>
        <Tooltip content="设置" placement="right">
          <Link href={solveOnly ? "/settings/appearance" : "/settings/agents"} className="workspace-rail-control" data-workspace-action="settings" aria-label="打开设置">
            <Icon name="gear" size={19} />
          </Link>
        </Tooltip>
      </div>
    </aside>
  );
}

export function WorkspaceFrame({ children }: { children: ReactNode }) {
  const pathname = usePathname();
  const compactScreen = useMediaQuery("(max-width: 768px)");
  const solveOnly = useSolveOnlyMode();
  const overview = useWorkspaceOverview(10000, solveOnly);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [sidebarWidth, setSidebarWidthState] = useState(RAIL_WIDTH_DEFAULT);
  const [sidebarWidthReady, setSidebarWidthReady] = useState(false);
  const [mobileSidebarOpen, setMobileSidebarOpen] = useState(false);

  useEffect(() => {
    try {
      setSidebarCollapsed(window.localStorage.getItem(CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY) === "1");
      const storedWidth = Number(window.localStorage.getItem(CONVERSATION_SIDEBAR_WIDTH_STORAGE_KEY));
      if (Number.isFinite(storedWidth) && storedWidth > 0) {
        setSidebarWidthState(clampRailWidth(storedWidth));
      }
    } catch {
      // Browser storage is optional; the default sidebar geometry still works.
    } finally {
      setSidebarWidthReady(true);
    }
  }, []);

  useEffect(() => {
    if (!sidebarWidthReady) return;
    try {
      window.localStorage.setItem(CONVERSATION_SIDEBAR_WIDTH_STORAGE_KEY, String(sidebarWidth));
    } catch {
      // Keep the current width for this session.
    }
  }, [sidebarWidth, sidebarWidthReady]);

  useEffect(() => {
    if (!pathname.startsWith("/chat")) setMobileSidebarOpen(false);
  }, [pathname]);

  const toggleSidebarCollapsed = useCallback(() => {
    setSidebarCollapsed((current) => {
      const next = !current;
      try {
        window.localStorage.setItem(CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY, next ? "1" : "0");
      } catch {
        // Keep the current state for this session.
      }
      return next;
    });
  }, []);

  const setSidebarWidth = useCallback((width: number) => {
    setSidebarWidthState(clampRailWidth(width, window.innerWidth));
  }, []);

  const conversationChrome = useMemo<ConversationChrome>(() => ({
    sidebarCollapsed,
    toggleSidebarCollapsed,
    sidebarWidth,
    setSidebarWidth,
    mobileSidebarOpen,
    setMobileSidebarOpen,
  }), [sidebarCollapsed, toggleSidebarCollapsed, sidebarWidth, setSidebarWidth, mobileSidebarOpen]);

  const sidebarExpanded = compactScreen ? mobileSidebarOpen : !sidebarCollapsed;
  const sidebarLabel = sidebarExpanded ? "收起对话导航" : "展开对话导航";
  const toggleConversationSidebar = () => {
    if (compactScreen) setMobileSidebarOpen((open) => !open);
    else toggleSidebarCollapsed();
  };

  const skipTargetId = workspaceSkipTargetId(pathname);
  const frameContentId = workspaceFrameContentId(pathname);

  return (
    <OverviewContext.Provider value={overview}>
      <ConversationChromeContext.Provider value={conversationChrome}>
      <div
        className="workspace-frame"
        data-solve-only={solveOnly ? "true" : undefined}
        data-conversation-layout={pathname.startsWith("/chat") ? "true" : undefined}
        data-conversation-sidebar-collapsed={sidebarCollapsed ? "true" : "false"}
        style={{ "--conv-sidebar-width": `${sidebarWidth}px` } as CSSProperties}
      >
        <a className="skip-link" href={`#${skipTargetId}`}>跳到主要内容</a>
        <WorkspaceNav overview={overview} sidebarToggle={pathname.startsWith("/chat") ? (
          <Tooltip content={sidebarLabel} placement="right">
            <button type="button" className="workspace-rail-control workspace-rail-sidebar-toggle" data-workspace-action="sidebar"
              aria-label={sidebarLabel} aria-expanded={sidebarExpanded} aria-controls="conversation-sidebar" onClick={toggleConversationSidebar}>
              <Icon name="panelLeft" size={19} />
            </button>
          </Tooltip>
        ) : null} />
        <div id={frameContentId} tabIndex={frameContentId ? -1 : undefined} className="workspace-frame-content" data-workspace-path={pathname}>
          {pathname.startsWith("/chat") ? <div className="workspace-shared-brand-dock">
            <Link href="/" className="workspace-nav-brand" aria-label="返回 Muteki 首页"><MutekiLogo size={30} wordmark decorative /></Link>
            <button type="button" className="workspace-brand-sidebar-toggle" aria-label={sidebarLabel}
              aria-expanded={sidebarExpanded} aria-controls="conversation-sidebar" onClick={toggleConversationSidebar}>
              <Icon name="panelLeft" size={17} />
            </button>
          </div> : null}
          {children}
        </div>
        <GlobalWorkspacePalette overview={overview} />
      </div>
      </ConversationChromeContext.Provider>
    </OverviewContext.Provider>
  );
}
