import { useEffect, useState } from 'react';
import { ArrowLeft, ArrowRight, ChartNoAxesCombined, CircleHelp, Crosshair, Home, Layers, LoaderCircle, Maximize2, MessagesSquare, Minimize2, Moon, PanelLeft, Plug, RefreshCw, Search, Settings, Sun, Target, Trophy, X } from 'lucide-react';
import { IconButton } from './ui';
import type { DesktopState, NavItem } from './types';

const desktop = window.mutekiDesktop;
const initial: DesktopState = { origin: 'http://127.0.0.1:3001', status: 'idle', message: '', configuring: true, platform: 'darwin', theme: 'light', items: [], active: '/', pageTitle: 'Muteki', canGoBack: false, canGoForward: false, search: false, sidebar: false, themeToggle: false, sidebarCollapsed: false, usage: false, settingsHref: '/settings/appearance' };
const icons = { chat: MessagesSquare, task: Crosshair, pentest: Target, competition: Trophy, extension: Layers };
const fallback: NavItem[] = [{ href: '/chat', label: '对话', icon: 'chat', badge: '' }, { href: '/ctf', label: 'CTF 工作台', icon: 'task', badge: '' }, { href: '/pentest', label: '渗透测试', icon: 'pentest', badge: '' }, { href: '/competitions', label: '比赛', icon: 'competition', badge: '' }];

export function App() {
  const [state, setState] = useState(initial);
  const [origin, setOrigin] = useState(initial.origin);
  const [error, setError] = useState('');
  const [help, setHelp] = useState(false);
  const connected = state.status === 'connected';
  const busy = state.status === 'connecting';
  useEffect(() => { const off = desktop.onState(setState); void desktop.getState().then(setState); return off; }, []);
  useEffect(() => setOrigin(state.origin), [state.origin]);
  useEffect(() => { document.documentElement.dataset.theme = state.theme; }, [state.theme]);
  async function perform(fn: () => Promise<unknown>) {
    setError('');
    try { await fn(); } catch (error) { setError(error instanceof Error ? error.message : '操作失败，请重试。'); }
  }
  function navigate(href: string) { void perform(() => desktop.navigate(href)); }
  function action(name: string) { void perform(() => desktop.action(name)); }
  return <div className="desktop-shell">
    <header className={`titlebar ${state.platform === 'darwin' ? 'mac' : ''}`}>
      <div className="window-navigation"><IconButton icon={ArrowLeft} label="返回" disabled={!connected || !state.canGoBack} onClick={() => action('back')} /><IconButton icon={ArrowRight} label="前进" disabled={!connected || !state.canGoForward} onClick={() => action('forward')} /><IconButton icon={PanelLeft} label={state.sidebarCollapsed ? '展开会话侧栏' : '收起会话侧栏'} disabled={!connected || !state.sidebar} onClick={() => action('sidebar')} /></div>
      <h1>{state.pageTitle}</h1><span className="server-name">{connected ? new URL(state.origin).host : ''}</span><IconButton icon={RefreshCw} label="刷新当前页面" disabled={!connected} onClick={() => action('reload')} />
      {state.platform !== 'darwin' && <div className="window-controls"><IconButton icon={Minimize2} label="最小化" onClick={() => void desktop.windowAction('minimize')} /><IconButton icon={Maximize2} label="最大化" onClick={() => void desktop.windowAction('maximize')} /><IconButton icon={X} label="关闭窗口" onClick={() => void desktop.windowAction('close')} /></div>}
    </header>
    <nav className="activity-rail" aria-label="工作区导航">
      <div className="rail-main"><IconButton icon={Home} label="首页" aria-current={state.active === '/' ? 'page' : undefined} disabled={!connected} onClick={() => navigate('/')} />
        {(state.items.length ? state.items : fallback).map(item => <div className="rail-item" key={item.href}><IconButton icon={icons[item.icon] || Layers} label={item.label} aria-current={state.active === item.href ? 'page' : undefined} disabled={!connected || !state.items.length} onClick={() => navigate(item.href)} />{item.badge && <span className="nav-badge" aria-label={`${item.badge} 项待处理`}>{item.badge}</span>}</div>)}
        {state.usage && <IconButton icon={ChartNoAxesCombined} label="全局用量" aria-current={state.active === '/usage' ? 'page' : undefined} onClick={() => navigate('/usage')} />}
        <div className="rail-divider" /><IconButton icon={Search} label="全局搜索（⌘K）" disabled={!connected || !state.search} onClick={() => action('search')} />
      </div>
      <div className="rail-bottom"><IconButton icon={state.theme === 'dark' ? Sun : Moon} label={state.theme === 'dark' ? '切换到亮色模式' : '切换到暗色模式'} disabled={!connected || !state.themeToggle} onClick={() => action('theme')} /><IconButton icon={Settings} label="设置" aria-current={state.active === 'settings' ? 'page' : undefined} disabled={!connected} onClick={() => navigate(state.settingsHref)} /><IconButton icon={Plug} label="服务连接" className={state.configuring ? 'active' : ''} onClick={() => { setHelp(false); void desktop.configure(); }} /><IconButton icon={CircleHelp} label="桌面端说明" onClick={() => { setHelp(true); void desktop.configure(); }} /></div>
    </nav>
    <main className="service-surface" aria-label="原有工作台页面">
      {state.configuring && <div className="connection-stage">
        {help ? <section className="help-panel"><h2>Muteki 桌面端</h2><p>原来的顶部导航已移到最左侧。对话、CTF、渗透测试、比赛和设置使用原有页面，页面内的功能和操作方式保持一致。</p><p>导航入口会跟随工作台的显示模式与扩展配置自动同步。右上角支持后退、前进和刷新。</p><p>⌘ / Ctrl + K 打开原有全局搜索，⌘ / Ctrl + B 切换会话侧栏。</p><button className="primary" onClick={() => { setHelp(false); if (connected) void desktop.resume(); }}>返回工作台</button></section> : <section className="connection-panel"><p className="eyebrow">MUTEKI DESKTOP</p><h2>连接你的工作台</h2><p className="intro">沿用现有的工作区，让导航更适合桌面。</p><form onSubmit={event => { event.preventDefault(); void perform(() => desktop.connect(origin)); }}><label htmlFor="service-origin">工作台地址</label><input id="service-origin" type="url" required value={origin} onChange={event => setOrigin(event.target.value)} placeholder="http://127.0.0.1:3001" disabled={busy} /><p className="hint">本机默认端口为 3001；远程服务使用 HTTPS。</p>{(error || state.message) && <p className="form-error" role="alert">{error || state.message}</p>}<div className="connection-actions"><button className="primary" disabled={busy}>{busy ? <><LoaderCircle className="spin" size={15} />正在连接…</> : state.status === 'error' ? '重新连接' : '连接工作台'}</button>{connected && <button className="secondary" type="button" onClick={() => void desktop.resume()}>返回工作台</button>}</div></form></section>}
      </div>}
    </main>
    {error && !state.configuring && <div className="rail-error" role="alert">{error}<button onClick={() => setError('')} aria-label="关闭错误提示"><X size={13} /></button></div>}
  </div>;
}
