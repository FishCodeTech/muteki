const { app, BrowserWindow, WebContentsView, Menu, dialog, ipcMain, protocol, session, shell, screen } = require('electron');
const fs = require('node:fs');
const path = require('node:path');
const { SHELL_URL, normalizeOrigin, parseWebUrl, sameOrigin, allowedNavigation, partitionFor, webPreferences } = require('./policy.cjs');
const { readPreferences, writePreferences } = require('./preferences.cjs');
const { WORKSPACE_CSS, READ_CHROME, ACTION_SCRIPTS, navigationScript, normalizeRoute, iconForRoute, activeRoute, itemsForSolveOnly } = require('./workspace-chrome.cjs');

app.setName('Muteki');
if (process.env.MUTEKI_DESKTOP_USER_DATA) app.setPath('userData', path.resolve(process.env.MUTEKI_DESKTOP_USER_DATA));
app.enableSandbox();
protocol.registerSchemesAsPrivileged([{ scheme: 'muteki-desktop', privileges: { standard: true, secure: true, supportFetchAPI: true } }]);

const TITLE_HEIGHT = 44, RAIL_WIDTH = 56;
const preferencesFile = path.join(app.getPath('userData'), 'connection.json');
const state = {
  origin: readPreferences(preferencesFile).origin, status: 'idle', message: '', configuring: true,
  platform: process.platform, theme: 'light', items: [], active: '/', pageTitle: 'Muteki',
  canGoBack: false, canGoForward: false, search: false, sidebar: false, themeToggle: false,
  sidebarCollapsed: false, usage: false, settingsHref: '/settings/appearance',
};
const configuredSessions = new Set();
const previews = new Set();
const lastRoutes = new Map();
let window, serviceView, pollTimer;
let connecting = false, polling = false, externalPrompt = false;
let connectionGeneration = 0;
// Last full-mode rail scraped from WorkspaceFrame; used when settings omits the frame.
let lastFullWorkspaceItems = [];

function emit(patch = {}) {
  Object.assign(state, patch);
  if (window && !window.isDestroyed()) window.webContents.send('desktop:state-changed', { ...state });
}
function layout() {
  if (!serviceView || !window) return;
  const [width, height] = window.getContentSize();
  serviceView.setBounds({ x: RAIL_WIDTH, y: TITLE_HEIGHT, width: Math.max(0, width - RAIL_WIDTH), height: Math.max(0, height - TITLE_HEIGHT) });
}
function configure() { emit({ configuring: true }); serviceView?.setVisible(false); window?.webContents.focus(); }
function resume() {
  if (!serviceView || state.status !== 'connected') return;
  emit({ configuring: false }); serviceView.setVisible(true); serviceView.webContents.focus();
}
function disposeService() {
  clearInterval(pollTimer); pollTimer = undefined;
  for (const preview of previews) if (!preview.isDestroyed()) preview.destroy();
  previews.clear();
  const old = serviceView; serviceView = undefined;
  if (!old) return;
  if (window && !window.isDestroyed()) window.contentView.removeChildView(old);
  if (!old.webContents.isDestroyed()) old.webContents.close();
}
async function openExternal(value) {
  const url = parseWebUrl(value);
  if (!url || externalPrompt || !window) return;
  externalPrompt = true;
  try {
    const { response } = await dialog.showMessageBox(window, { type: 'question', message: `在系统浏览器中打开 ${url.origin}？`, buttons: ['取消', '打开'], defaultId: 0, cancelId: 0 });
    if (response === 1) await shell.openExternal(url.href);
  } catch { /* Closing the app also closes pending native dialogs. */ }
  finally { externalPrompt = false; }
}
function guardContents(contents, origin, preview = false) {
  const guard = event => { if (!allowedNavigation(event.url, origin, { preview })) { event.preventDefault(); void openExternal(event.url); } };
  contents.on('will-navigate', guard);
  contents.on('will-redirect', event => { if (event.isMainFrame) guard(event); });
  contents.on('will-attach-webview', event => event.preventDefault());
  contents.setWindowOpenHandler(({ url }) => {
    if (!preview && allowedNavigation(url, origin, { preview: true })) return { action: 'allow', overrideBrowserWindowOptions: { width: 1000, height: 740, autoHideMenuBar: true, webPreferences: webPreferences(partitionFor(origin)) } };
    void openExternal(url); return { action: 'deny' };
  });
  contents.on('did-create-window', child => { previews.add(child); guardContents(child.webContents, origin, true); child.on('closed', () => previews.delete(child)); });
}
function configureSession(origin) {
  const partition = partitionFor(origin), browserSession = session.fromPartition(partition);
  if (configuredSessions.has(partition)) return partition;
  configuredSessions.add(partition);
  let notifications = false, prompting = false;
  const trusted = (contents, requestingOrigin) => Boolean(contents && sameOrigin(contents.getURL(), origin) && sameOrigin(requestingOrigin, origin));
  browserSession.setPermissionCheckHandler((contents, permission, requestingOrigin) => trusted(contents, requestingOrigin) && (permission === 'clipboard-sanitized-write' || (permission === 'notifications' && notifications)));
  browserSession.setPermissionRequestHandler(async (contents, permission, callback, details) => {
    if (!trusted(contents, details.requestingUrl) || details.isMainFrame === false) return callback(false);
    if (permission === 'clipboard-sanitized-write') return callback(true);
    if (permission !== 'notifications' || prompting) return callback(false);
    if (notifications) return callback(true);
    prompting = true;
    try {
      const { response } = await dialog.showMessageBox(window, { type: 'question', message: `允许 ${origin} 发送桌面通知？`, buttons: ['暂不允许', '允许'], defaultId: 0, cancelId: 0 });
      notifications = response === 1; callback(notifications);
    } catch { callback(false); } finally { prompting = false; }
  });
  browserSession.on('will-download', (_event, item) => item.setSaveDialogOptions({ title: '保存 Muteki 文件', defaultPath: path.join(app.getPath('downloads'), path.basename(item.getFilename())) }));
  return partition;
}

async function syncChrome() {
  const view = serviceView;
  if (!view || polling || view.webContents.isDestroyed() || !sameOrigin(view.webContents.getURL(), state.origin)) return;
  polling = true;
  try {
    const chrome = await view.webContents.executeJavaScript(READ_CHROME);
    if (view !== serviceView) return;
    let items = state.items;
    if (chrome.hasFrame) {
      items = chrome.items.flatMap(item => {
        try { const href = normalizeRoute(item.href, state.origin); return [{ href, label: String(item.label), badge: String(item.badge), icon: iconForRoute(href) }]; }
        catch { return []; }
      });
      if (!items.length && chrome.solveOnly) items = [{ href: '/task', label: '单题', badge: '', icon: 'task' }];
      // Remember full-mode entries so settings (no WorkspaceFrame) can restore chat/competition.
      if (!chrome.solveOnly && items.length) lastFullWorkspaceItems = items;
    } else {
      // Settings/share skip WorkspaceFrame; still sync the native rail from persisted solveOnly.
      items = itemsForSolveOnly(chrome.solveOnly, lastFullWorkspaceItems.length ? lastFullWorkspaceItems : state.items);
    }
    const active = activeRoute(chrome.pathname, items, chrome.activeHref);
    if (active && !['/', 'settings', '/usage'].includes(active)) lastRoutes.set(`${state.origin}:${active}`, new URL(view.webContents.getURL()).pathname + new URL(view.webContents.getURL()).search);
    const next = {
      items, active, theme: chrome.theme, search: chrome.search, sidebar: chrome.sidebar,
      sidebarCollapsed: chrome.sidebarCollapsed, themeToggle: chrome.themeToggle,
      settingsHref: normalizeRoute(chrome.settingsHref, state.origin), usage: chrome.hasFrame ? chrome.usage : !chrome.solveOnly,
      canGoBack: view.webContents.navigationHistory.canGoBack(), canGoForward: view.webContents.navigationHistory.canGoForward(),
      pageTitle: items.find(item => item.href === active)?.label || (active === 'settings' ? '设置' : active === '/usage' ? '全局用量' : active === '/' ? 'Muteki' : '工作台'),
    };
    if (Object.entries(next).some(([key, value]) => JSON.stringify(state[key]) !== JSON.stringify(value))) emit(next);
  } catch { /* Navigation can destroy the document during a metadata read. */ }
  finally { polling = false; }
}

async function connect(value) {
  if (connecting) throw new Error('正在连接，请稍候。');
  const origin = normalizeOrigin(value), attempt = ++connectionGeneration;
  connecting = true; disposeService();
  lastFullWorkspaceItems = [];
  emit({ origin, status: 'connecting', message: '', configuring: true, items: [], usage: false, search: false, sidebar: false, themeToggle: false, canGoBack: false, canGoForward: false });
  const view = new WebContentsView({ webPreferences: webPreferences(configureSession(origin)) });
  serviceView = view; window.contentView.addChildView(view); view.setVisible(false); layout(); guardContents(view.webContents, origin);
  const failed = message => { if (view !== serviceView) return; view.setVisible(false); emit({ status: 'error', message, configuring: true }); };
  view.webContents.on('dom-ready', () => { if (view === serviceView && sameOrigin(view.webContents.getURL(), origin)) void view.webContents.insertCSS(WORKSPACE_CSS).catch(() => {}); });
  view.webContents.on('did-navigate', () => void syncChrome());
  view.webContents.on('did-navigate-in-page', () => void syncChrome());
  view.webContents.on('page-title-updated', () => void syncChrome());
  view.webContents.on('did-fail-load', (_event, code, _description, _url, mainFrame) => { if (!connecting && mainFrame && code !== -3) failed('页面加载失败，请检查服务后重新连接。'); });
  view.webContents.on('render-process-gone', () => failed('工作台页面停止响应，请重新连接。'));
  let timeout;
  try {
    await Promise.race([view.webContents.loadURL(origin), new Promise((_resolve, reject) => { timeout = setTimeout(() => reject(new Error('timeout')), 20000); })]);
    if (attempt !== connectionGeneration || view !== serviceView) return;
    if (!sameOrigin(view.webContents.getURL(), origin)) throw new Error('redirect');
    await view.webContents.insertCSS(WORKSPACE_CSS);
    writePreferences(preferencesFile, origin); emit({ status: 'connected', message: '', configuring: false });
    view.setVisible(true); view.webContents.focus(); await syncChrome();
    pollTimer = setInterval(() => void syncChrome(), 700);
  } catch {
    if (view === serviceView) { disposeService(); emit({ status: 'error', message: '无法连接。请确认服务已启动、地址和证书有效。', configuring: true }); }
  } finally { clearTimeout(timeout); connecting = false; }
  return { ...state };
}

async function navigate(value) {
  const view = serviceView;
  if (!view || state.status !== 'connected') throw new Error('请先连接工作台。');
  const route = normalizeRoute(value, state.origin);
  const allowed = new Set(['/', '/usage', state.settingsHref, ...state.items.map(item => item.href)]);
  if (!allowed.has(route)) throw new Error('此导航入口当前不可用。');
  const target = lastRoutes.get(`${state.origin}:${route}`) || route;
  resume();
  if (new URL(view.webContents.getURL()).pathname + new URL(view.webContents.getURL()).search === target) return;
  // Click existing Next links when possible so the business application's
  // client routing and draft persistence remain its own responsibility.
  const clicked = await view.webContents.executeJavaScript(navigationScript(target), true);
  if (!clicked && view === serviceView) await view.webContents.loadURL(`${state.origin}${target}`);
  await syncChrome();
}
async function pageAction(name) {
  if (!serviceView || state.status !== 'connected') return;
  const contents = serviceView.webContents; resume();
  if (name === 'back' && contents.navigationHistory.canGoBack()) contents.navigationHistory.goBack();
  else if (name === 'forward' && contents.navigationHistory.canGoForward()) contents.navigationHistory.goForward();
  else if (name === 'reload') contents.reload();
  else if (ACTION_SCRIPTS[name]) await contents.executeJavaScript(ACTION_SCRIPTS[name], true);
  await syncChrome();
}

function installIPC() {
  const handle = (name, fn) => ipcMain.handle(name, async (event, ...args) => {
    if (!window || event.sender !== window.webContents || event.senderFrame !== window.webContents.mainFrame || event.senderFrame.url !== SHELL_URL) return { ok: false, error: 'Untrusted desktop IPC sender' };
    try { return { ok: true, value: await fn(...args) }; } catch (error) { return { ok: false, error: error.message || '操作失败。' }; }
  });
  handle('desktop:state', () => ({ ...state })); handle('desktop:connect', connect);
  handle('desktop:configure', configure); handle('desktop:resume', resume);
  handle('desktop:navigate', navigate); handle('desktop:action', pageAction);
  handle('desktop:window', name => { if (name === 'minimize') window.minimize(); if (name === 'maximize') window.isMaximized() ? window.unmaximize() : window.maximize(); if (name === 'close') window.close(); });
}
function installMenu() {
  const action = name => () => void pageAction(name).catch(() => {});
  Menu.setApplicationMenu(Menu.buildFromTemplate([
    ...(process.platform === 'darwin' ? [{ label: 'Muteki', submenu: [{ role: 'about' }, { type: 'separator' }, { label: '服务连接…', accelerator: 'CmdOrCtrl+,', click: configure }, { type: 'separator' }, { role: 'hide' }, { role: 'hideOthers' }, { role: 'unhide' }, { type: 'separator' }, { role: 'quit' }] }] : []),
    { label: '工作台', submenu: [{ label: '首页', click: () => void navigate('/').catch(() => {}) }, { label: '全局搜索', accelerator: 'CmdOrCtrl+K', click: action('search') }, { label: '服务连接…', click: configure }, { type: 'separator' }, { role: process.platform === 'darwin' ? 'close' : 'quit' }] },
    { label: '编辑', submenu: [{ role: 'undo' }, { role: 'redo' }, { type: 'separator' }, { role: 'cut' }, { role: 'copy' }, { role: 'paste' }, { role: 'selectAll' }] },
    { label: '视图', submenu: [{ label: '后退', accelerator: 'Alt+Left', click: action('back') }, { label: '前进', accelerator: 'Alt+Right', click: action('forward') }, { label: '刷新页面', accelerator: 'CmdOrCtrl+R', click: action('reload') }, { label: '切换会话侧栏', accelerator: 'CmdOrCtrl+B', click: action('sidebar') }, { type: 'separator' }, { role: 'resetZoom' }, { role: 'zoomIn' }, { role: 'zoomOut' }, { role: 'togglefullscreen' }] }, { role: 'windowMenu' },
  ]));
}
async function createWindow() {
  const area = screen.getPrimaryDisplay().workArea;
  window = new BrowserWindow({ title: 'Muteki', width: Math.min(1440, area.width - 40), height: Math.min(960, area.height - 40), minWidth: Math.min(900, area.width), minHeight: Math.min(580, area.height), show: false,
    titleBarStyle: 'hidden', ...(process.platform === 'darwin' ? { trafficLightPosition: { x: 16, y: 16 } } : {}), backgroundColor: '#f3f3f3',
    webPreferences: { ...webPreferences(undefined), preload: path.join(__dirname, 'preload.cjs') },
  });
  window.webContents.on('will-navigate', event => event.preventDefault()); window.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  window.on('resize', layout); window.on('close', () => { connectionGeneration++; disposeService(); }); window.on('closed', () => { window = undefined; });
  await window.loadURL(SHELL_URL); window.center(); window.show();
  const origin = process.env.MUTEKI_DESKTOP_URL || (fs.existsSync(preferencesFile) ? state.origin : '');
  if (origin) void connect(origin).catch(error => emit({ status: 'error', message: error.message }));
}
if (!app.requestSingleInstanceLock()) app.quit();
else {
  app.on('second-instance', () => { if (window) { if (window.isMinimized()) window.restore(); window.show(); window.focus(); } });
  app.on('window-all-closed', () => { if (process.platform !== 'darwin') app.quit(); });
  app.on('before-quit', disposeService); app.on('activate', () => { if (!window) void createWindow(); });
  app.whenReady().then(async () => {
    const root = path.resolve(__dirname, '../renderer-dist');
    const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png', '.svg': 'image/svg+xml' };
    protocol.handle('muteki-desktop', async request => {
      try {
        const url = new URL(request.url), file = path.resolve(root, `.${decodeURIComponent(url.pathname)}`), type = types[path.extname(file)];
        if (url.host !== 'app' || !file.startsWith(`${root}${path.sep}`) || !type) return new Response('Not found', { status: 404 });
        return new Response(await fs.promises.readFile(file), { headers: { 'Content-Type': `${type}; charset=utf-8`, 'Content-Security-Policy': "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'" } });
      } catch { return new Response('Not found', { status: 404 }); }
    });
    session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false)); session.defaultSession.setPermissionCheckHandler(() => false);
    installIPC(); installMenu(); await createWindow();
  }).catch(() => { dialog.showErrorBox('Muteki 无法启动', '请先构建桌面界面，或重新安装应用。'); app.quit(); });
}
