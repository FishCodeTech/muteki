import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ArrowLeft, ArrowRight, CircleHelp, Crosshair, Home, Layers, LoaderCircle, Maximize2, MessagesSquare, Minimize2, Moon, PanelLeft, Plug, RefreshCw, Search, Settings, Sun, Target, Trophy, X } from 'lucide-react';
import { ConversationShell, type ConversationNavigation } from '@/components/conversation/ConversationShell';
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
import { readSavedTheme, readSavedSelection, applySelection } from '@/lib/palette-engine';
import { IconButton } from './ui';
import type { DesktopState } from './types';
import { ConversationShortcutsHelp } from '@/components/conversation/ConversationShortcutsHelp';
import { NativeCapabilitiesPanel } from '@/components/NativeCapabilitiesPanel';

const WorkerSettings = lazy(() => import('@/components/WorkerOrchestration').then(module => ({ default: module.WorkerOrchestration })));
const Appearance = lazy(() => import('@/components/WorkerOrchestration').then(module => ({ default: module.AppearanceWorkspace })));
const AgentExtensions = lazy(() => import('./ChatExtensionsSettings').then(module => ({ default: module.ChatExtensionsSettings })));
const Notifications = lazy(() => import('@/components/NotificationSettings').then(module => ({ default: module.NotificationSettings })));
const desktop = window.mutekiDesktop;
const initial: DesktopState = { origin: '', transportOrigin: '', status: 'idle', message: '', configuring: true, platform: '', route: '/chat' };
const entries = [{ href: '/chat', label: ['对话', 'Chat'], icon: MessagesSquare }, { href: '/ctf', label: ['CTF 工作台', 'CTF workspace'], icon: Crosshair }, { href: '/pentest', label: ['渗透测试', 'Pentest'], icon: Target }, { href: '/competitions', label: ['比赛', 'Competitions'], icon: Trophy }];
const settingsEntries = [{href:'/settings/appearance',label:['外观和语言','Appearance and language']},{href:'/settings/agents',label:['Agents 和接入点','Agents and providers']},{href:'/settings/agent-extensions',label:['Agent 扩展','Agent extensions']},{href:'/settings/notifications',label:['通知','Notifications']},{href:'/settings/shortcuts',label:['聊天快捷键','Chat shortcuts']},{href:'/settings/desktop',label:['桌面能力','Desktop capabilities']}];

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
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false), [sidebarWidth, setSidebarWidth] = useState(280);
  const [mobileSidebarOpen, setMobileSidebarOpen] = useState(false), [theme, setTheme] = useState<'light' | 'dark'>(() => readSavedTheme());
  const [reload, setReload] = useState(0), [settingsQuery, setSettingsQuery] = useState('');
  const route = useMemo(() => new URL(state.route || '/chat', window.location.href), [state.route]);
  const settingsPathname = route.pathname === '/settings/chat-plugins' ? '/settings/agent-extensions' : route.pathname;
  const chat = route.pathname.startsWith('/chat'), settings = route.pathname.startsWith('/settings');
  const connected = state.status === 'connected', busy = state.status === 'connecting';
  const perform = useCallback(async (fn: () => Promise<unknown>) => {
    setError(''); try { await fn(); } catch (error) { if ((error as {code?: string})?.code === 'desktop.navigation_cancelled') return; setError(error instanceof Error ? error.message : '操作失败，请重试。'); }
  }, []);
  const go = useCallback((href: string, replace = false) => {
    void perform(async () => {
      await desktop.navigate(href, replace ? 'replace' : 'push');
    });
  }, [perform]);
  const navigation = useMemo<ConversationNavigation>(() => ({ router: { push: href => go(href), replace: href => go(href, true) }, pathname: route.pathname, searchParams: route.searchParams }), [go, route]);
  useEffect(() => {
    let updates = 0;
    const accept = (next: DesktopState) => {
      updates++;
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
  useEffect(() => { void desktop.setLocale(lang).catch(error => setError(error.message)); }, [lang]);
  useEffect(() => {
    const observer = new MutationObserver(() => { const current = document.documentElement.dataset.theme; if (current === 'light' || current === 'dark') setTheme(current); });
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] }); return () => observer.disconnect();
  }, []);
  useEffect(() => {
    const off = desktop.onCommand(({ name }) => {
      if (name === 'settings') go('/settings/appearance');
      else if (name === 'shortcuts') go('/settings/shortcuts');
      else if (name === 'sidebar') { if (stateRef.current.configuring) return; setSidebarCollapsed(value => !value); }
      else if (name === 'reload') setReload(value => value + 1);
      else if (name === 'theme') { const next = theme === 'dark' ? 'light' : 'dark'; try { localStorage.setItem('muteki.theme', next); } catch (error) { setError(String(error)); } applySelection(readSavedSelection(), next); document.documentElement.dataset.theme = next; }
      else if (chat && !stateRef.current.configuring) window.dispatchEvent(new CustomEvent('muteki:command', { detail: { name } }));
      else if (name === 'new-chat' || name === 'search') go('/chat');
    });
    return off;
  }, [chat, go, theme]);
  useEffect(() => desktop.onBeforeClose(({ token }) => {
    if (chat && state.transportOrigin && authOpen) window.dispatchEvent(new CustomEvent('muteki:before-close', { detail: { token } }));
    else void flushComposerAttachmentCache().then(cache => {
      const results = [cache, flushComposerDraftStore(), flushComposerRecallStore(), flushUserInputDraftStore(), flushQueueEditDraftStore(), flushSendIntentStore(), flushTaskReceiptStore()];
      return desktop.acknowledgeClose({ token, persisted: results.every(result => result.persisted), error: results.flatMap(result => result.error ? [result.error] : []).join('\n') });
    }).catch(error => { void desktop.acknowledgeClose({ token, persisted: false, error: error.message }).catch(() => {}); setError(error.message); });
  }), [chat, state.transportOrigin, authOpen]);
  const chrome = useMemo(() => ({ sidebarCollapsed, toggleSidebarCollapsed: () => setSidebarCollapsed(value => !value), sidebarWidth, setSidebarWidth, mobileSidebarOpen, setMobileSidebarOpen }), [sidebarCollapsed, sidebarWidth, mobileSidebarOpen]);
  const pageTitle = settings ? label('设置', 'Settings') : entries.find(item => route.pathname.startsWith(item.href))?.label[en ? 1 : 0] || 'Muteki';
  const noticeKey = `${state.transportOrigin}:${state.noticeVersion}:${state.persistenceWarning}:${state.message}`;
  const notice = error || (dismissedNotice !== noticeKey ? state.persistenceWarning || (!state.configuring ? state.message : '') : '');
  return <div className="desktop-shell">
    <header className={`titlebar ${state.platform === 'darwin' ? 'mac' : ''}`}>
      <div className="window-navigation"><IconButton icon={ArrowLeft} label={label('返回', 'Back')} disabled={!connected || !state.canGoBack} onClick={() => void perform(() => desktop.action('back'))} /><IconButton icon={ArrowRight} label={label('前进', 'Forward')} disabled={!connected || !state.canGoForward} onClick={() => void perform(() => desktop.action('forward'))} /><IconButton icon={PanelLeft} label={label('切换会话侧栏', 'Toggle chat sidebar')} disabled={!connected || !chat || state.configuring} onClick={() => void perform(() => desktop.action('sidebar'))} /></div>
      <h1 title={pageTitle}>{pageTitle}</h1><span className="server-name" title={state.origin}>{connected ? new URL(state.origin).host : ''}</span><IconButton icon={RefreshCw} label={label('刷新当前页面', 'Reload current page')} disabled={!connected} onClick={() => void perform(() => desktop.action('reload'))} />
      {state.platform !== 'darwin' && <div className="window-controls"><IconButton icon={Minimize2} label={label('最小化', 'Minimize')} onClick={() => void perform(() => desktop.windowAction('minimize'))} /><IconButton icon={Maximize2} label={label('最大化', 'Maximize')} onClick={() => void perform(() => desktop.windowAction('maximize'))} /><IconButton icon={X} label={label('关闭窗口', 'Close window')} onClick={() => void perform(() => desktop.windowAction('close'))} /></div>}
    </header>
    <nav className="activity-rail" aria-label={label('工作区导航', 'Workspace navigation')}><div className="rail-main"><IconButton icon={Home} label={label('新建对话', 'New chat')} disabled={!connected} onClick={() => go('/chat')} />{entries.map(item => <IconButton key={item.href} icon={item.icon} label={item.label[en ? 1 : 0]} aria-current={route.pathname.startsWith(item.href) ? 'page' : undefined} disabled={!connected} onClick={() => go(item.href)} />)}<div className="rail-divider" /><IconButton icon={Search} label={label('搜索对话', 'Search conversations')} disabled={!connected || !chat || state.configuring} onClick={() => void perform(() => desktop.action('search'))} /></div><div className="rail-bottom"><IconButton icon={theme === 'dark' ? Sun : Moon} label={label('切换主题', 'Toggle theme')} disabled={!connected} onClick={() => void perform(() => desktop.action('theme'))} /><IconButton icon={Settings} label={label('设置', 'Settings')} disabled={!connected} onClick={() => go('/settings/appearance')} /><IconButton icon={Plug} label={label('服务连接', 'Service connection')} onClick={() => { setHelp(false); void perform(() => desktop.configure()); }} /><IconButton icon={CircleHelp} label={label('桌面端说明', 'Desktop help')} onClick={() => { setHelp(true); void perform(() => desktop.configure()); }} /></div></nav>
    <main className="service-surface" aria-label={label('工作台', 'Workspace')}>
      {state.transportOrigin && <div className="local-workspace" hidden={state.configuring}>
        <LoginGate key={state.transportOrigin} onAuthStateChange={acceptAuthState}><ConversationChromeContext.Provider value={chrome}>
          {chat ? <ConversationShell key={`${state.transportOrigin}:${reload}`} threadId={decodeURIComponent(route.pathname.split('/')[2] || '')} navigation={navigation} /> : settings ? <div key={`${state.transportOrigin}:${reload}`} className="desktop-settings"><nav aria-label={label('设置分类', 'Settings categories')}><input aria-label={label('搜索设置', 'Search settings')} placeholder={label('搜索设置…', 'Search settings…')} value={settingsQuery} onChange={event => setSettingsQuery(event.target.value)} />{settingsEntries.filter(item => item.label.join(' ').toLowerCase().includes(settingsQuery.toLowerCase())).map(item => <button key={item.href} aria-current={settingsPathname === item.href ? 'page' : undefined} onClick={() => go(item.href)}>{item.label[en ? 1 : 0]}</button>)}</nav><Suspense fallback={<div role="status">{label('加载设置…', 'Loading settings…')}</div>}><div className="desktop-settings-content">{settingsPathname === '/settings/agents' ? <WorkerSettings navigation={navigation} defaultReturnTo="/chat" /> : settingsPathname === '/settings/agent-extensions' ? <AgentExtensions navigation={navigation} /> : settingsPathname === '/settings/notifications' ? <Notifications /> : settingsPathname === '/settings/shortcuts' ? <ConversationShortcutsHelp open onClose={() => { if (state.canGoBack) void perform(() => desktop.action('back')); else go('/settings/appearance', true); }} /> : settingsPathname === '/settings/desktop' ? <NativeCapabilitiesPanel /> : <Appearance clientContext="desktop" />}</div></Suspense></div> : null}
        </ConversationChromeContext.Provider></LoginGate>
      </div>}
      {state.configuring && <div className="connection-stage">{help ? <section className="help-panel"><h2>Muteki Desktop</h2><p>{label('对话、搜索和设置在本地窗口运行。预览在隔离的页面中打开；项目路径需要区分桌面客户端和服务端宿主。', 'Chat, search, and settings run in this local window. Previews are isolated. Project paths distinguish the desktop client from the service host.')}</p><button className="primary" onClick={() => { setHelp(false); if (connected) void perform(() => desktop.resume()); }}>{label('返回工作台', 'Return to workspace')}</button></section> : <section className="connection-panel"><p className="eyebrow">MUTEKI DESKTOP</p><h2>{label('连接你的工作台', 'Connect your workspace')}</h2><form onSubmit={event => { event.preventDefault(); void perform(() => desktop.connect(origin)); }}><label htmlFor="service-origin">{label('工作台地址', 'Workspace address')}</label><input id="service-origin" type="url" required value={origin} onChange={event => setOrigin(event.target.value)} disabled={busy} /><p className="hint">{label('本机默认端口为 3001；远程服务使用 HTTPS。', 'Local default port: 3001. Remote services use HTTPS.')}</p>{(error || state.message) && <p className="form-error" role="alert">{error || state.message}</p>}<div className="connection-actions"><button className="primary" disabled={busy}>{busy ? <><LoaderCircle className="spin" size={15} />{label('正在连接…', 'Connecting…')}</> : label('连接工作台', 'Connect')}</button>{(connected || state.draftRecovery) && <button className="secondary" type="button" onClick={() => { setError(''); void perform(() => desktop.resume()); }}>{state.draftRecovery ? label('查看保留草稿', 'View preserved drafts') : label('返回上个可用工作台', 'Return to previous workspace')}</button>}</div></form></section>}</div>}
    </main>
    {notice && <div className="desktop-notice" role="alert">{notice}<button onClick={() => { setError(''); setDismissedNotice(noticeKey); }} aria-label={label('关闭错误提示', 'Dismiss error')}><X size={15} /></button></div>}
  </div>;
}
