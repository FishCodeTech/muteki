import { invalidateAuth } from "@/lib/serviceAuth";
import { rehydrateDesktopBridge } from "@/lib/desktopChatBridge";
import { Activity, useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { ArrowLeft, ArrowRight, ChartNoAxesCombined, CircleHelp, Crosshair, Layers, LoaderCircle, Maximize2, MessagesSquare, Minimize2, Moon, PanelLeft, Plug, RefreshCw, Search, Settings, Shield, Sun, Target, Trophy, X } from 'lucide-react';
import { ConversationShell, type ConversationNavigation } from '@/components/conversation/ConversationShell';
import { SrcTaskShell } from '@/components/src/SrcTaskShell';
import { SrcHome } from '@/components/src/SrcHome';
import { ConversationChromeContext } from '@/components/conversationChrome';
import { LoginGate } from '@/components/LoginGate';
import { resetAuthForServiceChange } from '@/lib/useRun';
import { useLang } from '@/lib/i18n';
import { flushComposerAttachmentCache, flushComposerDraftStore } from '@/lib/composerDraftStore';
import { flushComposerRecallStore } from '@/lib/composerRecallStore';
import { flushUserInputDraftStore } from '@/lib/userInputDraftStore';
import { flushQueueEditDraftStore } from '@/lib/queueEditDraftStore';
import { flushSendIntentStore } from '@/lib/sendIntentStore';
import { flushTaskReceiptStore } from '@/lib/task-center';
import { readSavedSelection, readSavedTheme } from '@/lib/palette-engine';
import { IconButton } from './ui';
import type { DesktopState } from './types';
import { DesktopSettings } from './settings/DesktopSettings';
import { readThemePreference, subscribeThemePreference } from '@/lib/themePreference';
import { refreshUiPreferences } from '@/lib/uiPreferences';
import { applyTheme } from '@/components/settings/AppearanceSettings';
import { clampRailWidth, CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY, CONVERSATION_SIDEBAR_WIDTH_STORAGE_KEY, RAIL_WIDTH_DEFAULT } from '@/lib/railSizing';

const readStored = (key: string) => { try { return window.localStorage.getItem(key); } catch { return null; } };
const writeStored = (key: string, value: string) => { try { window.localStorage.setItem(key, value); } catch { /* keep the in-memory value */ } };

const desktop = rehydrateDesktopBridge(window.mutekiDesktop);
const isLoopbackOrigin = (origin: string) => { try { return ['127.0.0.1', 'localhost', '[::1]'].includes(new URL(origin).hostname); } catch { return false; } };
const initial: DesktopState = { origin: '', transportOrigin: '', status: 'idle', message: '', configuring: true, platform: '', route: '/chat' };
const entries = [{ href: '/chat', label: ['对话', 'Chat'], icon: MessagesSquare }, { href: '/ctf', label: ['CTF 工作台', 'CTF workspace'], icon: Crosshair }, { href: '/pentest', label: ['渗透测试', 'Pentest'], icon: Target }, { href: '/src', label: ['SRC 赏金', 'SRC bounty'], icon: Shield }, { href: '/competitions', label: ['比赛', 'Competitions'], icon: Trophy }, { href: '/usage', label: ['全局用量', 'Usage'], icon: ChartNoAxesCombined }];

export function App() {
  const { lang } = useLang(), en = lang === 'en';
  const label = (zh: string, english: string) => en ? english : zh;
  const [state, setState] = useState(initial), [origin, setOrigin] = useState('http://127.0.0.1:3001');
  const stateRef = useRef(state); stateRef.current = state;
  const [error, setError] = useState(''), [help, setHelp] = useState(false);
  const [dismissedNotice, setDismissedNotice] = useState('');
  const [authStatus, setAuthStatus] = useState({open: false, transportOrigin: ''});
  const authOpen = authStatus.open && authStatus.transportOrigin === state.transportOrigin;
  const acceptAuthState = useCallback((open: boolean) => setAuthStatus({open, transportOrigin: state.transportOrigin}), [state.transportOrigin]);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(() => readStored(CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY) === '1');
  const [sidebarWidth, setSidebarWidthState] = useState(() => {
    const stored = Number(readStored(CONVERSATION_SIDEBAR_WIDTH_STORAGE_KEY));
    return Number.isFinite(stored) && stored > 0 ? clampRailWidth(stored, window.innerWidth) : RAIL_WIDTH_DEFAULT;
  });
  const setSidebarWidth = useCallback((width: number) => setSidebarWidthState(clampRailWidth(width, window.innerWidth)), []);
  useEffect(() => { writeStored(CONVERSATION_SIDEBAR_WIDTH_STORAGE_KEY, String(sidebarWidth)); }, [sidebarWidth]);
  useEffect(() => { writeStored(CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY, sidebarCollapsed ? '1' : '0'); }, [sidebarCollapsed]);
  const [mobileSidebarOpen, setMobileSidebarOpen] = useState(false), [theme, setTheme] = useState<'light' | 'dark'>(() => readSavedTheme());
  const [reload, setReload] = useState(0);
  const [headerSlot, setHeaderSlot] = useState<HTMLDivElement | null>(null);
  const route = useMemo(() => new URL(state.route || '/chat', window.location.href), [state.route]);
  const workspaceMode = state.workspaceMode || (route.pathname.startsWith('/run') ? 'ctf' : route.pathname.split('/')[1]);
  const chat = workspaceMode === 'chat', src = workspaceMode === 'src', settings = workspaceMode === 'settings';
  const connected = state.status === 'connected', busy = state.status === 'connecting';
  const headerActive = (chat || src) && connected && !state.configuring && authOpen;
  const perform = useCallback(async (fn: () => Promise<unknown>) => {
    setError(''); try { await fn(); } catch (error) { if ((error as {code?: string})?.code === 'desktop.navigation_cancelled') return; setError(error instanceof Error ? error.message : '操作失败，请重试。'); }
  }, []);
  const go = useCallback((href: string, replace = false) => {
    void perform(async () => {
      await desktop.navigate(href, replace ? 'replace' : 'push');
    });
  }, [perform]);
  const activate = useCallback((href: string) => {
    void perform(() => desktop.navigate(href, 'activate'));
  }, [perform]);
  const navigation = useMemo<ConversationNavigation>(() => ({ router: { push: href => go(href), replace: href => go(href, true) }, pathname: route.pathname, searchParams: route.searchParams, hash: state.anchorTarget?.hash, anchorRequestId: state.anchorTarget?.id, consumeAnchor: (id: number) => { void desktop.consumeAnchor(id).catch(error => setError(error.message)); } }), [go, route, state.anchorTarget]);
  const workspaceKey = `${state.transportOrigin}:${reload}`;
  const chatVisible = chat && !state.configuring;
  const [retainedChat, setRetainedChat] = useState<{ key: string; navigation: ConversationNavigation; threadId: string } | null>(null);
  // Capture chat routes only while they are visible. Remote mode navigation must
  // not turn the retained conversation into an empty draft in the background.
  if (state.transportOrigin && chatVisible && (retainedChat?.key !== workspaceKey || retainedChat.navigation !== navigation)) {
    setRetainedChat({ key: workspaceKey, navigation, threadId: decodeURIComponent(route.pathname.split('/')[2] || '') });
  }
  const mountedChat = retainedChat?.key === workspaceKey ? retainedChat : null;
  const settingsVisible = settings && !state.configuring;
  const [retainedSettings, setRetainedSettings] = useState<{ key: string; navigation: ConversationNavigation; pathname: string } | null>(null);
  if (state.transportOrigin && settingsVisible && (retainedSettings?.key !== workspaceKey || retainedSettings.navigation !== navigation)) {
    setRetainedSettings({ key: workspaceKey, navigation, pathname: route.pathname });
  }
  const mountedSettings = retainedSettings?.key === workspaceKey ? retainedSettings : null;
  const srcVisible = src && !state.configuring;
  const [retainedSrc, setRetainedSrc] = useState<{ key: string; navigation: ConversationNavigation; taskId: string } | null>(null);
  if (state.transportOrigin && srcVisible && (retainedSrc?.key !== workspaceKey || retainedSrc.navigation !== navigation)) {
    setRetainedSrc({ key: workspaceKey, navigation, taskId: decodeURIComponent(route.pathname.split('/')[3] || '') });
  }
  const mountedSrc = retainedSrc?.key === workspaceKey ? retainedSrc : null;
  useEffect(() => {
    let updates = 0;
    const accept = (next: DesktopState) => {
      updates++;
      if (next.authSessionVersion !== stateRef.current.authSessionVersion && next.transportOrigin === stateRef.current.transportOrigin) invalidateAuth("peer_changed");
      if (next.transportOrigin && next.transportOrigin !== stateRef.current.transportOrigin) {
        resetAuthForServiceChange(next.origin, next.transportOrigin);
        setMobileSidebarOpen(false);
      }
      stateRef.current = next; setState(next);
    };
    const off = desktop.onState(accept);
    void desktop.getState().then(next => { if (updates === 0) accept(next); }).catch(error => setError(error.message));
    return off;
  }, []);
  useEffect(() => { if (state.origin) setOrigin(state.origin); }, [state.origin]);
  useEffect(() => desktop.onFocus(() => { void refreshUiPreferences(); }), []);
  useEffect(() => { void desktop.setLocale(lang).catch(error => setError(error.message)); }, [lang]);
  useEffect(() => {
    const observer = new MutationObserver(() => { const current = document.documentElement.dataset.theme; if (current === 'light' || current === 'dark') setTheme(current); });
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] }); return () => observer.disconnect();
  }, []);
  useEffect(() => {
    // Remote pages (CTF, pentest, competitions) lost their own theme toggle with the web rail, so they follow this shell.
    let last = '';
    const sync = () => {
      const payload = { preference: readThemePreference(), resolvedTheme: readSavedTheme(), selection: readSavedSelection() };
      const key = JSON.stringify(payload);
      if (key === last) return;
      last = key;
      void desktop.syncAppearance(payload).catch(error => { last = ''; setError(error.message); });
    };
    sync();
    const offTheme = subscribeThemePreference(sync);
    const observer = new MutationObserver(sync);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme', 'data-scheme', 'style'] });
    return () => { observer.disconnect(); offTheme(); };
  }, []);
  useEffect(() => {
    const off = desktop.onCommand(({ name }) => {
      if (name === 'settings') activate('/settings');
      else if (name === 'shortcuts') go('/settings/shortcuts');
      else if (name === 'sidebar') {
        if (stateRef.current.configuring) return;
        // Below the shell's 769px breakpoint the sidebar is an overlay drawer that ignores sidebarCollapsed.
        if (window.matchMedia('(max-width: 768px)').matches) setMobileSidebarOpen(value => !value);
        else setSidebarCollapsed(value => !value);
      }
      else if (name === 'reload') { void refreshUiPreferences(); setReload(value => value + 1); }
      else if (name === 'theme') applyTheme(theme === 'dark' ? 'light' : 'dark');
      else if ((chat || src) && !stateRef.current.configuring) window.dispatchEvent(new CustomEvent('muteki:command', { detail: { name } }));
      else if (name === 'new-chat' || name === 'search') go('/chat');
    });
    return off;
  }, [chat, src, go, activate, theme]);
  const hasConversationShell = chat || (src && Boolean(route.pathname.split('/')[3]));
  useEffect(() => desktop.onBeforeClose(({ token }) => {
    if (hasConversationShell && state.transportOrigin && authOpen) window.dispatchEvent(new CustomEvent('muteki:before-close', { detail: { token } }));
    else void flushComposerAttachmentCache().then(cache => {
      const results = [cache, flushComposerDraftStore(), flushComposerRecallStore(), flushUserInputDraftStore(), flushQueueEditDraftStore(), flushSendIntentStore(), flushTaskReceiptStore()];
      return desktop.acknowledgeClose({ token, persisted: results.every(result => result.persisted), error: results.flatMap(result => result.error ? [result.error] : []).join('\n') });
    }).catch(error => { void desktop.acknowledgeClose({ token, persisted: false, error: error.message }).catch(() => {}); setError(error.message); });
  }), [hasConversationShell, state.transportOrigin, authOpen]);
  const chrome = useMemo(() => ({ sidebarCollapsed, toggleSidebarCollapsed: () => setSidebarCollapsed(value => !value), sidebarWidth, setSidebarWidth, mobileSidebarOpen, setMobileSidebarOpen, headerSlot: chat || src ? headerSlot : null }), [sidebarCollapsed, sidebarWidth, setSidebarWidth, mobileSidebarOpen, headerSlot, chat, src]);
  const pageTitle = settings ? label('设置', 'Settings') : entries.find(item => item.href === `/${workspaceMode}`)?.label[en ? 1 : 0] || 'Muteki';
  const noticeKey = `${state.transportOrigin}:${state.noticeVersion}:${state.persistenceWarning}:${state.message}`;
  const notice = error || (dismissedNotice !== noticeKey ? state.persistenceWarning || (!state.configuring ? state.message : '') : '');
  return <div className="desktop-shell" style={{ '--titlebar-sidebar-width': `${(chat || src) && !sidebarCollapsed ? sidebarWidth : 0}px` } as CSSProperties}>
    <header className={`titlebar ${state.platform === 'darwin' ? 'mac' : ''}`}>
      <div className="window-navigation"><IconButton icon={ArrowLeft} label={label('返回', 'Back')} disabled={!connected || !state.canGoBack} onClick={() => void perform(() => desktop.action('back'))} /><IconButton icon={ArrowRight} label={label('前进', 'Forward')} disabled={!connected || !state.canGoForward} onClick={() => void perform(() => desktop.action('forward'))} /><IconButton icon={PanelLeft} label={label('切换会话侧栏', 'Toggle chat sidebar')} disabled={!connected || !(chat || src) || state.configuring} onClick={() => void perform(() => desktop.action('sidebar'))} /></div>
      {state.environment?.channel === 'dev' && <span className="environment-label dev">DEV</span>}{state.environment?.channel === 'candidate' && <span className="environment-label candidate">CANDIDATE</span>}
      <h1 title={pageTitle}>{pageTitle}</h1><div className="titlebar-thread" ref={setHeaderSlot} hidden={!headerActive} inert={!headerActive} aria-hidden={!headerActive} />{connected && !isLoopbackOrigin(state.origin) && <span className="server-name" title={state.origin}>{new URL(state.origin).host}</span>}
      {state.platform !== 'darwin' && <div className="window-controls"><IconButton icon={Minimize2} label={label('最小化', 'Minimize')} onClick={() => void perform(() => desktop.windowAction('minimize'))} /><IconButton icon={Maximize2} label={label('最大化', 'Maximize')} onClick={() => void perform(() => desktop.windowAction('maximize'))} /><IconButton icon={X} label={label('关闭窗口', 'Close window')} onClick={() => void perform(() => desktop.windowAction('close'))} /></div>}
    </header>
    <nav className="activity-rail" aria-label={label('工作区导航', 'Workspace navigation')}><div className="rail-main">{entries.map(item => <IconButton key={item.href} icon={item.icon} label={item.label[en ? 1 : 0]} aria-current={item.href === `/${workspaceMode}` ? 'page' : undefined} disabled={!connected} onClick={() => activate(item.href)} />)}<div className="rail-divider" /><IconButton icon={Search} label={src ? label('搜索研究', 'Search research') : label('搜索对话', 'Search conversations')} disabled={!connected || !(chat || src) || state.configuring} onClick={() => void perform(() => desktop.action('search'))} /></div><div className="rail-bottom"><IconButton icon={RefreshCw} label={label('刷新当前页面', 'Reload current page')} disabled={!connected} onClick={() => void perform(() => desktop.action('reload'))} /><IconButton icon={theme === 'dark' ? Sun : Moon} label={label('切换主题', 'Toggle theme')} disabled={!connected} onClick={() => void perform(() => desktop.action('theme'))} /><IconButton icon={Settings} label={label('设置', 'Settings')} disabled={!connected} onClick={() => activate('/settings')} /><IconButton icon={Plug} label={label('服务连接', 'Service connection')} onClick={() => { setHelp(false); void perform(() => desktop.configure()); }} /><IconButton icon={CircleHelp} label={label('桌面端说明', 'Desktop help')} onClick={() => { setHelp(true); void perform(() => desktop.configure()); }} /></div></nav>
    <main className="service-surface" aria-label={label('工作台', 'Workspace')}>
      {state.transportOrigin && <div className="local-workspace" hidden={state.configuring} style={{ '--conv-sidebar-width': `${sidebarWidth}px` } as CSSProperties}>
        <LoginGate key={state.transportOrigin} onAuthStateChange={acceptAuthState}><ConversationChromeContext.Provider value={chrome}>
          <Activity key={workspaceKey} mode={chatVisible ? 'visible' : 'hidden'}>
            {mountedChat && <ConversationShell threadId={mountedChat.threadId} navigation={mountedChat.navigation} />}
          </Activity>
          <Activity key={`src:${workspaceKey}`} mode={srcVisible ? 'visible' : 'hidden'}>
            <ConversationChromeContext.Provider value={chrome}>
              {mountedSrc && (mountedSrc.taskId ? <SrcTaskShell taskId={mountedSrc.taskId} navigation={mountedSrc.navigation} /> : <SrcHome navigation={mountedSrc.navigation} onOpenTask={taskId => go(`/src/tasks/${encodeURIComponent(taskId)}`)} />)}
            </ConversationChromeContext.Provider>
          </Activity>
          <Activity key={`settings:${workspaceKey}`} mode={settingsVisible ? 'visible' : 'hidden'}>
            {mountedSettings && <DesktopSettings pathname={mountedSettings.pathname} navigation={mountedSettings.navigation} go={go} origin={state.origin} connected={connected} onConfigure={() => { setHelp(false); void perform(() => desktop.configure()); }} onHelp={() => { setHelp(true); void perform(() => desktop.configure()); }} onBack={() => { if (state.canGoBack) void perform(() => desktop.action('back')); else activate('/chat'); }} />}
          </Activity>
        </ConversationChromeContext.Provider></LoginGate>
      </div>}
      {state.configuring && <div className="connection-stage">{help ? <section className="help-panel"><h2>Muteki Desktop</h2><p>{label('对话、搜索和设置在本地窗口运行。预览在隔离的页面中打开；项目路径需要区分桌面客户端和服务端宿主。', 'Chat, search, and settings run in this local window. Previews are isolated. Project paths distinguish the desktop client from the service host.')}</p><button className="primary" onClick={() => { setHelp(false); if (connected) void perform(() => desktop.resume()); }}>{label('返回工作台', 'Return to workspace')}</button></section> : <section className="connection-panel"><p className="eyebrow">MUTEKI DESKTOP</p><h2>{label('打开你的工作台', 'Open your workspace')}</h2>
        <div className="local-service-choice">
          <button className="primary" disabled={busy} onClick={() => void perform(() => desktop.connectLocal())}>
            {busy && <LoaderCircle className="spin" size={15} />}{label(state.managedService?.state === 'starting' ? '正在启动本机工作台…' : '使用本机工作台', state.managedService?.state === 'starting' ? 'Starting local workspace…' : 'Use local workspace')}
          </button>
          <p className="hint">{label('自动启动配套服务，数据保存在此 App 的独立环境中。', 'Starts the bundled service with data in this App’s separate environment.')}</p>
          {state.managedService?.state === 'failed' && <p className="form-error" role="alert">{state.managedService.error}<br />{label('日志：', 'Logs: ')}{state.managedService.logs}</p>}
        </div>
        <form onSubmit={event => { event.preventDefault(); void perform(() => desktop.connect(origin)); }}><label htmlFor="service-origin">{label('或连接已有服务', 'Or connect an existing service')}</label><input id="service-origin" type="url" required value={origin} onChange={event => setOrigin(event.target.value)} disabled={busy} /><p className="hint">{label('本机默认端口为 3001；已有服务支持 HTTP 或 HTTPS。', 'Local default port: 3001. Existing services support HTTP or HTTPS.')}</p>{(error || state.message) && <p className="form-error" role="alert">{error || state.message}</p>}<div className="connection-actions"><button className="primary" disabled={busy}>{busy ? <><LoaderCircle className="spin" size={15} />{label('正在连接…', 'Connecting…')}</> : label('连接工作台', 'Connect')}</button>{(connected || state.draftRecovery) && <button className="secondary" type="button" onClick={() => { setError(''); void perform(() => desktop.resume()); }}>{state.draftRecovery ? label('查看保留草稿', 'View preserved drafts') : label('返回上个可用工作台', 'Return to previous workspace')}</button>}</div></form></section>}</div>}
    </main>
    {notice && <div className="desktop-notice" role="alert">{notice}<button onClick={() => { setError(''); setDismissedNotice(noticeKey); }} aria-label={label('关闭错误提示', 'Dismiss error')}><X size={15} /></button></div>}
  </div>;
}
