const { app, BrowserWindow, WebContentsView, Menu, Notification: NativeNotification, dialog, ipcMain, protocol, session, shell, screen, systemPreferences } = require('electron');
const fs = require('node:fs');
const path = require('node:path');
const { randomUUID } = require('node:crypto');
const { SHELL_URL, normalizeOrigin, normalizeDesktopRoute, parseWebUrl, partitionFor, webPreferences } = require('./policy.cjs');
const { readPreferences, writePreferences } = require('./preferences.cjs');
const { DesktopError, transportError, errorResponse, probeService, forwardService, closeScope } = require('./transport.cjs');
const { saveAttachment, restoreAttachment, removeAttachment, cacheUsage } = require('./attachment-cache.cjs');
const { NativeSpeech } = require('./speech.cjs');

app.setName('Muteki');
if (process.env.MUTEKI_DESKTOP_USER_DATA) app.setPath('userData', path.resolve(process.env.MUTEKI_DESKTOP_USER_DATA));
app.enableSandbox();
protocol.registerSchemesAsPrivileged([{ scheme: 'muteki-desktop', privileges: { standard: true, secure: true, supportFetchAPI: true, stream: true, corsEnabled: true } }]);
const preferencesFile = path.join(app.getPath('userData'), 'connection.json');
let preferences = readPreferences(preferencesFile), language = 'zh', quitting = false;
const windows = new Map(), scopes = new Map(), documents = new Map();
const nativeSpeech = new NativeSpeech({ platform: process.platform,
  load: () => require(app.isPackaged ? path.join(process.resourcesPath, 'native', 'macos-speech.node') : path.join(__dirname, '..', 'native', 'build', 'macos-speech.node')),
  send: (record, event) => record.window.webContents.send('desktop:speech', event),
});
const cacheRoot = path.join(app.getPath('userData'), 'attachment-drafts');
let cacheWrite = Promise.resolve();
const CAPABILITIES = Object.freeze({ version: 1, host: 'desktop-client', localChat: true,
  managedWindows: true, isolatedPreview: true, nativePathSelection: true,
  serverPathMapping: false, terminalTransport: true, attachmentCache: true,
  microphone: process.platform === 'darwin', notifications: true,
  entries: {
    pathSelection: { supported: true, host: 'desktop-client' },
    workspaceFileActions: { supported: true, host: 'desktop-client', code: 'desktop.mapping_manual', reason: '需要用户选择本机对应目录；文件同步尚未验证。' },
    preview: { supported: true, host: 'desktop-client' },
    attachmentCache: { supported: true, host: 'desktop-client' },
    terminal: { supported: true, host: 'service' },
    microphone: { supported: process.platform === 'darwin', host: 'desktop-client', ...(process.platform !== 'darwin' ? { code: 'desktop.microphone_os_settings', reason: '通过系统设置管理麦克风权限。' } : {}) },
    notifications: { supported: true, host: 'desktop-client' },
    deepLinks: { supported: app.isPackaged, host: 'desktop-client', ...(!app.isPackaged ? { code: 'desktop.packaged_only', reason: '开发运行不注册系统聊天协议。' } : {}) },
  } });
const local = value => { try { const u = new URL(value); return u.protocol === 'muteki-desktop:' && u.host === 'app'; } catch { return false; } };
const text = (zh, en) => language === 'en' ? en : zh;
function emit(record, patch = {}) {
  if (patch.route && patch.route !== record.state.route) nativeSpeech.cancel(record);
  if (patch.message || patch.persistenceWarning) patch.noticeVersion = (record.state.noticeVersion || 0) + 1;
  Object.assign(record.state, patch);
  if (!record.window.isDestroyed()) record.window.webContents.send('desktop:state-changed', { ...record.state });
}
function focused() { return windows.get(BrowserWindow.getFocusedWindow()?.id) || windows.values().next().value; }
function activeThread(record, threadId) {
  try {
    const route = new URL(record.state.route, SHELL_URL);
    return !record.remote && route.pathname.startsWith('/chat/') && decodeURIComponent(route.pathname.split('/')[2] || '') === threadId;
  } catch { return false; }
}
function command(name, record = focused()) {
  if (!record || record.window.isDestroyed()) return;
  if (record.state.configuring && ['search', 'sidebar'].includes(name)) return;
  record.window.webContents.focus();
  record.window.webContents.send('desktop:command', { name });
}
function resetNavigation(record, route) {
  record.navigation = { entries: [route], index: 0 };
  return { route, canGoBack: false, canGoForward: false };
}
function commitRoute(record, route, mode, historyIndex) {
  const history = record.navigation || { entries: [record.state.route], index: 0 };
  if (mode === 'traverse') history.index = historyIndex;
  else if (mode === 'replace') history.entries[history.index] = route;
  else if (history.entries[history.index] !== route) {
    history.entries = [...history.entries.slice(0, history.index + 1), route];
    history.index = history.entries.length - 1;
  }
  record.navigation = history;
  emit(record, { route, configuring: false, canGoBack: history.index > 0, canGoForward: history.index < history.entries.length - 1 });
}
async function executeCommand(name, record = focused()) {
  if (!record) return;
  const preview = record.preview;
  if (preview && !preview.hidden && preview.webContents.isFocused() && ['back', 'forward', 'reload'].includes(name)) {
    if (name === 'reload') preview.webContents.reload();
    else if (name === 'back' && preview.webContents.navigationHistory.canGoBack()) preview.webContents.navigationHistory.goBack();
    else if (name === 'forward' && preview.webContents.navigationHistory.canGoForward()) preview.webContents.navigationHistory.goForward();
    return;
  }
  if (name === 'back' || name === 'forward') {
    const history = record.navigation;
    const next = history && history.index + (name === 'back' ? -1 : 1);
    if (history?.entries[next]) await navigate(record, history.entries[next], 'traverse', next);
    return;
  }
  if (name === 'reload' && record.remote && !record.remote.webContents.isDestroyed()) {
    record.remote.webContents.reload();
    return;
  }
  if (name === 'reload' && record.scope && (record.state.route.startsWith('/chat') || record.state.route.startsWith('/settings'))) {
    const scope = record.scope, route = record.state.route, generation = record.generation;
    const saved = await persistenceBarrier(record, 'reload');
    if (!saved.persisted) throw new DesktopError('desktop.draft_save_failed', saved.error || '草稿未保存，刷新已暂停。', { retryable: true });
    if (record.window.isDestroyed() || scope !== record.scope || scope.closed || generation !== record.generation || route !== record.state.route) throw new DesktopError('desktop.navigation_cancelled', '刷新所属的工作台或会话已经改变。');
  }
  command(name, record);
}
function installKeyboard(record, contents) {
  contents.on('before-input-event', (event, input) => {
    if (input.type !== 'keyDown' || input.isComposing || input.isAutoRepeat) return;
    let name;
    if ((input.meta || input.control) && !input.alt) {
      const key = input.key.toLowerCase();
      if (!input.shift) name = ({ ',': 'settings', k: 'search', b: 'sidebar', r: 'reload' })[key];
      else if (key === 'n') name = 'new-chat';
    } else if (input.alt && !input.shift && !input.control && !input.meta) {
      name = ({ ArrowLeft: 'back', ArrowRight: 'forward' })[input.key];
    }
    if (!name) return;
    // One native command owns both menu and focused-renderer shortcuts.
    event.preventDefault();
    void executeCommand(name, record).catch(error => emit(record, { message: error.message, error: transportError(error) }));
  });
}
function disposePreview(record) {
  if (!record.preview) return;
  const preview = record.preview; record.preview = undefined;
  if (!record.window.isDestroyed()) record.window.contentView.removeChildView(preview);
  if (!preview.webContents.isDestroyed()) preview.webContents.close();
}
function disposeRemote(record) {
  if (!record.remote) return;
  const remote = record.remote; record.remote = undefined;
  if (!record.window.isDestroyed()) record.window.contentView.removeChildView(remote);
  if (!remote.webContents.isDestroyed()) remote.webContents.close();
}
function disposeConnection(record) {
  nativeSpeech.cancel(record);
  closeScope(record.scope);
  disposeNotifications(record, 'desktop.notification_scope_closed');
  if (record.scope) scopes.delete(record.scope.host);
  record.scope = undefined;
  for (const socket of record.terminals.values()) socket.close();
  record.terminals.clear(); disposePreview(record); disposeRemote(record);
  record.workspaceGrants.clear();
  record.notificationPermission = undefined;
  record.notificationDiagnostic = undefined;
  record.microphoneGranted = false;
  for (const [host, document] of documents) if (document.record === record) documents.delete(host);
}
function persist(record) {
  try {
    if (!record.state.origin) return;
    preferences = { ...preferences, origin: record.state.origin, bounds: record.window.getNormalBounds() };
    if (record.scope?.serviceId && record.scope.identityId) {
      const key = `${record.scope.origin}|${record.scope.serviceId}|${record.scope.identityId}`;
      preferences.routes = { ...preferences.routes, [key]: record.state.route };
    }
    writePreferences(preferencesFile, preferences.origin, preferences);
    emit(record, { persistenceWarning: '' });
  } catch (error) { emit(record, { persistenceWarning: text('当前工作台可继续使用，但桌面偏好保存失败。', 'The workspace remains available, but desktop preferences could not be saved.'), persistenceError: transportError(error) }); }
}
async function connect(record, value) {
  if (record.connecting) throw new DesktopError('desktop.connection_busy', text('正在连接，请稍候。', 'A connection is already in progress.'), { retryable: true });
  const origin = normalizeOrigin(value), attempt = ++record.generation;
  const previous = record.scope;
  record.connecting = true;
  const controller = new AbortController(); record.connectionAbort = controller;
  const timeout = setTimeout(() => controller.abort(), 20000);
  emit(record, { status: 'connecting', candidateOrigin: origin, message: '', error: undefined, configuring: true });
  try {
    await probeService(origin, controller.signal);
    if (record.window.isDestroyed() || attempt !== record.generation) throw new DesktopError('desktop.connection_cancelled', '连接已取消。');
    // Commit the candidate only after it is usable. The previous scope remains
    // live on a failed probe, including its authentication and private drafts.
    if (previous) {
      const saved = await persistenceBarrier(record, 'switch');
      if (!saved.persisted) throw new DesktopError('desktop.draft_save_failed', saved.error || '草稿尚未安全保存，服务连接保持不变。', { retryable: true });
    }
    if (record.window.isDestroyed() || attempt !== record.generation || record.scope !== previous) throw new DesktopError('desktop.connection_cancelled', '连接已取消。');
    const host = `service-${record.window.id}-${attempt}`;
    disposeConnection(record);
    const scope = { origin, host, version: attempt, requests: new Set(), closed: false };
    record.scope = scope; scopes.set(host, { record, scope });
    record.navigationGeneration = (record.navigationGeneration || 0) + 1;
    record.routeRequested = false;
    emit(record, { origin, transportOrigin: `muteki-desktop://${host}`, connectionVersion: attempt,
      status: 'connected', configuring: false, ...resetNavigation(record, '/chat'), serviceId: '', identityId: '', message: '', candidateOrigin: '', draftRecovery: false });
    persist(record);
  } catch (cause) {
    const error = cause instanceof DesktopError ? cause : new DesktopError('desktop.connection_failed', text('连接失败，请重试。', 'Connection failed. Please retry.'), { retryable: true, cause });
    if (!record.window.isDestroyed() && attempt === record.generation) emit(record, {
      status: previous && !previous.closed ? 'connected' : 'error', configuring: true,
      message: error.message, error: transportError(error), canResume: Boolean(previous && !previous.closed) });
    throw error;
  } finally { clearTimeout(timeout); record.connecting = false; record.connectionAbort = undefined; }
  return { ...record.state };
}
async function openExternal(record, value) {
  const url = parseWebUrl(value);
  if (!url) throw new DesktopError('desktop.external_url_invalid', '外链地址无效。');
  const { response } = await dialog.showMessageBox(record.window, { type: 'question', message: text('在系统浏览器中打开以下地址？', 'Open this address in your browser?'), detail: url.href,
    buttons: [text('取消', 'Cancel'), text('打开', 'Open')], defaultId: 0, cancelId: 0 });
  if (response === 1) await shell.openExternal(url.href);
}
function safeRoute(value) {
  return normalizeDesktopRoute(value);
}
function remoteBounds(record) {
  if (!record.remote || record.window.isDestroyed()) return;
  const [width, height] = record.window.getContentSize();
  record.remote.setBounds({ x: 56, y: 44, width: Math.max(0, width - 56), height: Math.max(0, height - 44) });
}
async function navigate(record, value, mode = 'push', historyIndex) {
  const route = safeRoute(value) === '/' ? '/chat' : safeRoute(value);
  if (!['push', 'replace', 'traverse'].includes(mode) || (mode === 'traverse' && (!Number.isInteger(historyIndex) || record.navigation?.entries[historyIndex] !== route))) throw new DesktopError('desktop.navigation_invalid', '导航历史请求无效。');
  record.routeRequested = true;
  if (route === record.state.route && !record.state.configuring && !record.navigationPending) {
    if (mode === 'traverse' && historyIndex !== record.navigation?.index) { commitRoute(record, route, mode, historyIndex); persist(record); }
    return;
  }
  const generation = record.navigationGeneration = (record.navigationGeneration || 0) + 1;
  record.navigationPending = false;
  disposePreview(record);
  if (/^\/(chat|settings)(\/|\?|$)/.test(route) || route === '/') {
    disposeRemote(record); commitRoute(record, route, mode, historyIndex);
    persist(record); return;
  }
  if (!record.scope) throw new DesktopError('desktop.not_connected', '请先连接工作台。');
  disposeRemote(record);
  let remote;
  try {
    const origin = record.scope.origin, partition = partitionFor(origin);
    const remoteSession = session.fromPartition(partition);
    const permitsClipboard = (contents, permission, requestingOrigin, details) =>
      permission === 'clipboard-sanitized-write' && details?.isMainFrame !== false &&
      parseWebUrl(requestingOrigin)?.origin === origin && parseWebUrl(contents?.getURL())?.origin === origin &&
      Array.from(windows.values()).some(owner => owner.remote?.webContents === contents && owner.scope?.origin === origin && !owner.scope.closed);
    remoteSession.setPermissionCheckHandler(permitsClipboard);
    remoteSession.setPermissionRequestHandler((contents, permission, callback, details) =>
      callback(permitsClipboard(contents, permission, details?.requestingUrl, details)));
    remote = new WebContentsView({ webPreferences: webPreferences(partition) });
    record.remote = remote; record.window.contentView.addChildView(remote); remoteBounds(record);
    record.navigationPending = true;
    guardRemote(record, remote.webContents, record.scope.origin);
    await remote.webContents.loadURL(record.scope.origin + route);
    if (record.remote !== remote || record.scope.closed || generation !== record.navigationGeneration) throw new DesktopError('desktop.navigation_cancelled', '该导航已由较新的请求替代。');
    const destination = new URL(remote.webContents.getURL());
    const loadedRoute = safeRoute(destination.pathname + destination.search + destination.hash);
    commitRoute(record, route, mode, historyIndex);
    if (loadedRoute !== route) commitRoute(record, loadedRoute, 'replace');
    persist(record);
  } catch (cause) {
    if (record.remote !== remote) throw new DesktopError('desktop.navigation_cancelled', '该导航已由较新的请求替代。');
    disposeRemote(record); throw new DesktopError('desktop.page_load_failed', '工作台页面加载失败，请重试。', { retryable: true, cause });
  } finally { if (generation === record.navigationGeneration) record.navigationPending = false; }
}
function guardRemote(record, contents, origin, preview = false) {
  installKeyboard(record, contents);
  const guard = event => {
    const url = parseWebUrl(event.url);
    if (!url || (!preview && url.origin !== origin)) { event.preventDefault(); if (url) void openExternal(record, url.href).catch(error => emit(record, { message: error.message })); }
    else if (!preview && /^\/(chat|settings)(\/|$)/.test(url.pathname)) { event.preventDefault(); void navigate(record, url.pathname + url.search).catch(error => emit(record, { message: error.message })); }
  };
  contents.on('will-navigate', guard); contents.on('will-redirect', event => { if (event.isMainFrame) guard(event); });
  if (!preview) {
    const navigated = (_event, value, isMainFrame = true) => {
      if (isMainFrame === false || record.remote?.webContents !== contents || record.navigationPending || record.scope?.closed) return;
      const url = parseWebUrl(value);
      if (!url || url.origin !== origin) return;
      const route = safeRoute(url.pathname + url.search + url.hash);
      if (/^\/(chat|settings)(\/|\?|$)/.test(route)) {
        void navigate(record, route).catch(error => emit(record, { message: error.message }));
      } else if (route !== record.state.route) {
        commitRoute(record, route, 'push'); persist(record);
      }
    };
    contents.on('did-navigate', (event, url) => navigated(event, url));
    contents.on('did-navigate-in-page', navigated);
  }
  contents.on('will-attach-webview', event => event.preventDefault());
  contents.setWindowOpenHandler(({ url }) => {
    const parsed = parseWebUrl(url);
    if (!preview && parsed?.origin === origin && /^\/chat(?:\/[^/]+)?$/.test(parsed.pathname)) void createWindow(origin, parsed.pathname + parsed.search).catch(error => emit(record, { message: error.message }));
    else if (parsed) void openExternal(record, parsed.href).catch(error => emit(record, { message: error.message }));
    return { action: 'deny' };
  });
}
async function openPreview(record, input) {
  if (!record.scope || record.scope.closed || !input || typeof input !== 'object') throw new DesktopError('desktop.preview_invalid', '预览请求无效或工作台连接已改变。');
  const url = parseWebUrl(input.url);
  if (!url) throw new DesktopError('desktop.preview_url_invalid', '预览仅支持 HTTP/HTTPS 地址。');
  const rect = input.rect;
  if (!rect || !['x', 'y', 'width', 'height'].every(key => Number.isFinite(rect[key]) && rect[key] >= 0)) throw new DesktopError('desktop.preview_bounds_invalid', '预览区域无效。');
  if (typeof input.surfaceId !== 'string' || !input.surfaceId || typeof input.threadId !== 'string' || !activeThread(record, input.threadId)) throw new DesktopError('desktop.preview_scope_invalid', '预览必须属于当前聊天视图。');
  const bounds = { x: Math.round(rect.x), y: Math.round(rect.y), width: Math.round(rect.width), height: Math.round(rect.height) };
  const previous = record.preview;
  if (previous && previous.scope === record.scope && previous.surfaceId === input.surfaceId && previous.threadId === input.threadId && previous.url === url.href) {
    previous.setBounds(bounds); previous.setVisible(true); previous.hidden = false;
    if (input.reload) previous.webContents.reload();
    return { id: previous.id };
  }
  disposePreview(record);
  const partition = `muteki-preview-${record.window.id}-${randomUUID()}`;
  const previewSession = session.fromPartition(partition);
  previewSession.setPermissionRequestHandler((_c, _p, callback) => callback(false));
  previewSession.setPermissionCheckHandler(() => false);
  const preview = new WebContentsView({ webPreferences: webPreferences(partition) });
  // Explicit session beats defaultSession; previews never inherit the trusted
  // renderer's storage, preload, custom transport, or native bridge.
  Object.assign(preview, { id: randomUUID(), surfaceId: input.surfaceId, threadId: input.threadId, scope: record.scope, url: url.href, hidden: false });
  record.preview = preview; record.window.contentView.addChildView(preview);
  preview.setBounds(bounds);
  const update = (status, message = '') => {
    if (record.preview === preview && !record.window.isDestroyed()) record.window.webContents.send('desktop:preview-state', { id: preview.id, threadId: preview.threadId, url: preview.webContents.getURL() || preview.url, title: preview.webContents.getTitle(), status, message,
      canGoBack: preview.webContents.navigationHistory.canGoBack(), canGoForward: preview.webContents.navigationHistory.canGoForward() });
  };
  preview.webContents.on('did-start-loading', () => update('loading'));
  preview.webContents.on('did-stop-loading', () => update('loaded'));
  preview.webContents.on('did-navigate', (_event, target) => { preview.url = target; update('loaded'); });
  preview.webContents.on('did-navigate-in-page', (_event, target) => { preview.url = target; update('loaded'); });
  preview.webContents.on('page-title-updated', () => update('loaded'));
  guardRemote(record, preview.webContents, url.origin, true);
  preview.webContents.on('render-process-gone', () => { if (record.preview !== preview) return; disposePreview(record); emit(record, { message: '预览进程已停止，请重新打开。' }); });
  void preview.webContents.loadURL(url.href).catch(() => update('error', '预览加载失败，请重试或在系统浏览器打开。'));
  return { id: preview.id };
}
function persistenceBarrier(record, purpose) {
  return new Promise(resolve => {
    const token = randomUUID();
    const timer = setTimeout(() => { record.barriers.delete(token); resolve({ persisted: false, error: '草稿保存未确认，请重试。' }); }, 5000);
    record.barriers.set(token, { resolve, timer });
    record.window.webContents.send('desktop:before-close', { token, purpose });
  });
}
function restoreBounds() {
  const area = screen.getPrimaryDisplay().workArea, candidate = preferences.bounds;
  const fallback = { width: Math.min(1440, area.width), height: Math.min(960, area.height), x: area.x, y: area.y };
  if (!candidate || !['x', 'y', 'width', 'height'].every(key => Number.isFinite(candidate[key]))) return fallback;
  const display = screen.getDisplayMatching(candidate).workArea;
  const width = Math.min(Math.max(360, candidate.width), display.width), height = Math.min(Math.max(320, candidate.height), display.height);
  return { width, height, x: Math.min(Math.max(candidate.x, display.x), display.x + display.width - width), y: Math.min(Math.max(candidate.y, display.y), display.y + display.height - height) };
}
async function createWindow(origin = '', route = '/chat') {
  let record;
  try {
    const window = new BrowserWindow({ title: 'Muteki', ...restoreBounds(), minWidth: 360, minHeight: 320, show: false,
      titleBarStyle: 'hidden', ...(process.platform === 'darwin' ? { trafficLightPosition: { x: 16, y: 16 } } : {}), backgroundColor: '#f3f3f3',
      webPreferences: { ...webPreferences(undefined), preload: path.join(__dirname, 'preload.cjs') } });
    record = { window, generation: 0, connecting: false, terminals: new Map(), selectedPaths: new Map(), workspaceGrants: new Map(), barriers: new Map(), state: {
      origin: origin || preferences.origin || '', transportOrigin: '', status: 'idle', configuring: true,
      message: preferences.error?.message || '', error: preferences.error, route: safeRoute(route), platform: process.platform, capabilities: CAPABILITIES,
    } };
    Object.assign(record.state, resetNavigation(record, record.state.route));
    if (route !== '/chat') record.pendingRoute = safeRoute(route);
    windows.set(window.id, record);
    installKeyboard(record, window.webContents);
    window.webContents.on('did-start-navigation', details => { if (details.isMainFrame) nativeSpeech.cancel(record); });
    window.webContents.on('will-navigate', event => {
      event.preventDefault();
      if (local(event.url)) { const target = new URL(event.url); void navigate(record, target.pathname + target.search).catch(error => emit(record, { message: error.message })); }
      else { const url = parseWebUrl(event.url); if (url) void openExternal(record, url.href).catch(error => emit(record, { message: error.message })); }
    });
    window.webContents.on('will-frame-navigate', event => {
      if (event.isMainFrame) return;
      try {
        const url = new URL(event.url), document = documents.get(url.host);
        if (url.protocol !== 'muteki-desktop:' || !document || document.record !== record || document.scope !== record.scope || url.pathname !== '/document.html') event.preventDefault();
      } catch { event.preventDefault(); }
    });
    window.webContents.setWindowOpenHandler(({ url }) => { if (local(url)) { const target = new URL(url); void createWindow(record.state.origin, target.pathname + target.search).catch(error => emit(record, { message: error.message })); }
      else if (parseWebUrl(url)) void openExternal(record, url).catch(error => emit(record, { message: error.message })); return { action: 'deny' }; });
    window.webContents.on('render-process-gone', () => { disposeConnection(record); emit(record, { status: 'error', configuring: true, message: '聊天界面进程已停止，请重新连接。' }); });
    window.on('resize', () => { remoteBounds(record); if (record.preview) { record.preview.setVisible(false); record.preview.hidden = true; } });
    window.on('close', event => {
      if (record.allowClose) return;
      event.preventDefault();
      if (record.pendingClose) return;
      const token = randomUUID(); record.pendingClose = token;
      window.webContents.send('desktop:before-close', { token });
      record.closeTimer = setTimeout(() => { record.pendingClose = undefined; quitting = false; emit(record, { message: text('草稿保存尚未确认，窗口仍保持打开。请重试关闭。', 'Draft saving was not confirmed. The window remains open; retry closing.') }); }, 5000);
    });
    window.on('closed', () => { clearTimeout(record.closeTimer); for (const barrier of record.barriers.values()) { clearTimeout(barrier.timer); barrier.resolve({ persisted: false, error: '窗口已关闭。' }); } record.barriers.clear(); record.generation++; record.connectionAbort?.abort(); disposeConnection(record); windows.delete(window.id); if (quitting && windows.size === 0) app.quit(); });
    await window.loadURL(SHELL_URL); window.show();
    if (origin || preferences.origin) await connect(record, origin || preferences.origin);
    return record;
  } catch (error) {
    if (record && !record.window.isDestroyed() && !record.window.isVisible()) { record.allowClose = true; record.window.destroy(); }
    else if (record && !record.window.isDestroyed()) emit(record, { configuring: true, message: error.message });
    if (!record || record.window.isDestroyed()) throw error;
    return record;
  }
}
function identity(record, scope, data) {
  if (record.scope !== scope || scope.closed) return;
  const changed = scope.serviceId && (scope.serviceId !== data.service_id || scope.identityId !== data.identity_id);
  if (changed) {
    // Stop old transport immediately, preserving the old scope's draft cache
    // identity until the renderer has confirmed its local save barrier.
    closeScope(scope); disposeNotifications(record, 'desktop.notification_scope_closed'); disposePreview(record); disposeRemote(record); record.workspaceGrants.clear();
    record.connectionAbort?.abort();
    for (const socket of record.terminals.values()) socket.close(); record.terminals.clear();
    const attempt = ++record.generation;
    void persistenceBarrier(record, 'identity').then(saved => {
      if (record.window.isDestroyed() || record.scope !== scope || record.generation !== attempt) return;
      if (!saved.persisted) {
        emit(record, { status: 'error', configuring: false, draftRecovery: true, message: saved.error || '服务身份已经改变。原草稿仍保留在本窗口，请保存后重新连接。' });
        return;
      }
      disposeConnection(record);
      const host = `service-${record.window.id}-${attempt}`;
      const next = { origin: scope.origin, host, version: attempt, requests: new Set(), closed: false, serviceId: data.service_id, identityId: data.identity_id };
      record.scope = next; scopes.set(host, { record, scope: next });
      record.navigationGeneration = (record.navigationGeneration || 0) + 1;
      emit(record, { transportOrigin: `muteki-desktop://${host}`, connectionVersion: attempt, serviceId: next.serviceId, identityId: next.identityId,
        status: 'connected', configuring: false, candidateOrigin: '', ...resetNavigation(record, '/chat'), draftRecovery: false, message: '服务数据身份已改变，请确认当前服务。原服务草稿按身份隔离保留。' });
    }).catch(error => emit(record, { status: 'error', draftRecovery: true, message: error.message }));
    return;
  }
  const firstIdentity = !scope.serviceId;
  scope.serviceId = data.service_id; scope.identityId = data.identity_id;
  const key = `${scope.origin}|${scope.serviceId}|${scope.identityId}`;
  const route = record.pendingRoute || (!changed && preferences.routes?.[key]) || '/chat'; record.pendingRoute = undefined;
  emit(record, { serviceId: scope.serviceId, identityId: scope.identityId, ...(firstIdentity && !record.routeRequested ? resetNavigation(record, safeRoute(route)) : {}) });
  persist(record);
}
function notificationScope(record, input) {
  const scope = record.scope;
  if (!scope || scope.closed || record.window.isDestroyed() || !scope.serviceId || !scope.identityId
    || input?.connectionVersion !== scope.version || input?.serviceId !== scope.serviceId || input?.identityId !== scope.identityId) {
    throw new DesktopError('desktop.notification_scope_changed', text('通知授权所属的工作台已改变，请在当前工作台重试。', 'The notification workspace changed. Retry in the current workspace.'));
  }
  return scope;
}
function notificationStatus(record, input) {
  const scope = notificationScope(record, input);
  const supported = NativeNotification.isSupported();
  const choice = record.notificationPermission?.scope === scope ? record.notificationPermission.allowed : undefined;
  return { connectionVersion: scope.version, serviceId: scope.serviceId, identityId: scope.identityId,
    permission: !supported ? 'unsupported' : choice === true ? 'granted' : choice === false ? 'denied' : 'default',
    workspacePermission: choice === true ? 'granted' : choice === false ? 'denied' : 'default',
    host: 'desktop-client', systemPermission: 'unknown',
    ...(record.notificationDiagnostic?.scope === scope ? { delivery: record.notificationDiagnostic.value } : {}),
    code: !supported ? 'desktop.notifications_unsupported' : choice === true ? 'desktop.notifications_workspace_allowed' : choice === false ? 'desktop.notifications_workspace_denied' : 'desktop.notifications_not_requested' };
}
async function requestNotifications(record, input) {
  const scope = notificationScope(record, input), status = notificationStatus(record, input);
  if (status.permission !== 'default') return status;
  if (record.notificationPrompt) {
    if (record.notificationPrompt.scope === scope) return record.notificationPrompt.promise;
    throw new DesktopError('desktop.notification_request_pending', text('上一个工作台的通知授权窗口尚未关闭，请先完成该选择。', 'Finish the previous workspace notification dialog first.'), { retryable: true });
  }
  const pending = { scope };
  record.notificationPrompt = pending;
  pending.promise = (async () => {
    try {
      const { response } = await dialog.showMessageBox(record.window, { type: 'question',
        message: text('允许此工作台发送桌面通知？', 'Allow desktop notifications from this workspace?'),
        detail: `${scope.origin}\n${text('系统通知权限由操作系统单独管理。', 'The operating system manages its notification permission separately.')}`,
        buttons: [text('暂不允许', 'Not now'), text('允许', 'Allow')], defaultId: 0, cancelId: 0 });
      if (notificationScope(record, input) !== scope) throw new DesktopError('desktop.notification_scope_changed', 'Notification workspace changed.');
      if (response !== 0 && response !== 1) throw new DesktopError('desktop.notification_reply_invalid', 'Notification permission dialog returned an invalid choice.');
      record.notificationPermission = { scope, allowed: response === 1 };
      return notificationStatus(record, input);
    } catch (error) {
      if (error instanceof DesktopError) throw error;
      throw new DesktopError('desktop.notification_request_failed', error.message || String(error), { retryable: true });
    } finally { if (record.notificationPrompt === pending) record.notificationPrompt = undefined; }
  })();
  return pending.promise;
}
function notificationsAllowed(record) {
  return Boolean(record.scope && !record.scope.closed && record.scope.serviceId && record.scope.identityId
    && record.notificationPermission?.scope === record.scope && record.notificationPermission.allowed === true);
}
function notificationEvent(entry, status, code, message) {
  const { record, scope } = entry;
  entry.history.push({ status, at: new Date().toISOString(), ...(code ? { code } : {}), ...(message ? { message } : {}) });
  const value = { connectionVersion: scope.version, serviceId: scope.serviceId, identityId: scope.identityId,
    id: entry.id, threadId: entry.threadId, eventId: entry.eventId, dedupeKey: entry.dedupeKey,
    seq: record.notificationSequence = (record.notificationSequence || 0) + 1, status, shown: entry.shown,
    ...(code ? { code } : {}), ...(message ? { message } : {}), history: entry.history.map(item => ({ ...item })) };
  entry.last = value;
  if (record.scope === scope) record.notificationDiagnostic = { scope, value };
  // No message contents or credentials in the diagnostic; OS errors remain complete.
  (status === 'failed' ? console.error : console.info)('muteki.notification', JSON.stringify(value));
  if (!record.window.isDestroyed()) record.window.webContents.send('desktop:notification-state', value);
  return value;
}
function releaseNotification(entry, closeNative = false) {
  if (!entry.active) return;
  entry.active = false;
  clearTimeout(entry.timer);
  if (entry.record.notifications?.get(entry.threadId) === entry) entry.record.notifications.delete(entry.threadId);
  if (closeNative && entry.note) {
    try { entry.note.close(); }
    catch (error) { notificationEvent(entry, 'failed', 'desktop.notification_close_failed', error.stack || error.message || String(error)); }
  }
  entry.note = undefined;
}
function disposeNotifications(record, code) {
  for (const entry of [...(record.notifications?.values() || [])]) {
    notificationEvent(entry, 'closed', code, text('通知所属工作台已结束，原生通知引用已释放。', 'The notification workspace ended; the native notification reference was released.'));
    releaseNotification(entry, true);
  }
}
function sendNotification(record, input) {
  const scope = notificationScope(record, input);
  if (!notificationsAllowed(record)) throw new DesktopError('desktop.notifications_workspace_denied', text('此工作台尚未获得通知授权。', 'This workspace has not been allowed to send notifications.'));
  if (!input || ['threadId', 'eventId', 'dedupeKey', 'title', 'body'].some(key => typeof input[key] !== 'string') || !input.threadId || !input.eventId || !input.dedupeKey) {
    throw new DesktopError('desktop.notification_input_invalid', 'The notification is missing its thread or event identity.');
  }
  const route = safeRoute(`/chat/${encodeURIComponent(input.threadId)}`);
  if (!NativeNotification.isSupported()) throw new DesktopError('desktop.notifications_unsupported', 'Native notifications are unsupported on this system.');
  record.notifications ||= new Map();
  const previous = record.notifications.get(input.threadId);
  if (previous?.scope === scope && previous.eventId === input.eventId && previous.dedupeKey === input.dedupeKey) return previous.last;
  if (previous) {
    notificationEvent(previous, 'closed', 'desktop.notification_replaced', 'Replaced by a new notification from this thread.');
    releaseNotification(previous, true);
  }
  const entry = { id: randomUUID(), record, scope, threadId: input.threadId, eventId: input.eventId,
    dedupeKey: input.dedupeKey, history: [], shown: false, active: true };
  record.notifications.set(entry.threadId, entry);
  notificationEvent(entry, 'submitted');
  const sameScope = () => record.scope === scope && !scope.closed && !record.window.isDestroyed()
    && input.serviceId === scope.serviceId && input.identityId === scope.identityId;
  const current = () => entry.active && sameScope();
  try {
    entry.note = new NativeNotification({ title: input.title, body: input.body });
    entry.note.on('show', () => {
      if (!current()) return;
      clearTimeout(entry.timer); entry.shown = true;
      notificationEvent(entry, 'shown');
    });
    entry.note.on('failed', (_event, error) => {
      if (!entry.active) return;
      notificationEvent(entry, 'failed', 'desktop.notification_failed', typeof error === 'string' ? error : error?.stack || error?.message || JSON.stringify(error));
      releaseNotification(entry, true);
    });
    entry.note.on('close', () => {
      if (!entry.active) return;
      notificationEvent(entry, 'closed', entry.shown ? undefined : 'desktop.notification_closed_before_show');
      releaseNotification(entry);
    });
    entry.note.on('click', () => {
      if (!current()) return;
      // A real click proves delivery even if this platform omitted its show event.
      clearTimeout(entry.timer); entry.shown = true;
      if (record.window.isMinimized()) record.window.restore();
      record.window.show(); record.window.focus();
      notificationEvent(entry, 'clicked');
      void navigate(record, route).then(() => {
        if (!current()) return;
        releaseNotification(entry, true);
      }).catch(error => {
        if (!current()) { console.error('muteki.notification.navigation', JSON.stringify({ id: entry.id, threadId: entry.threadId, code: 'desktop.notification_navigation_failed', message: error.stack || error.message || String(error) })); return; }
        notificationEvent(entry, 'failed', 'desktop.notification_navigation_failed', error.stack || error.message || String(error));
        releaseNotification(entry, true);
      });
    });
    entry.timer = setTimeout(() => {
      if (current() && !entry.shown) notificationEvent(entry, 'outcome_unknown', 'desktop.notification_show_unknown',
        text('系统在 10 秒内未返回显示或失败回执；投递结果未知，不会自动重发。', 'The system returned no show or failure receipt within 10 seconds. Delivery is unknown; it will not be retried automatically.'));
    }, 10000);
    entry.note.show();
    if (entry.active && entry.last.status === 'submitted') notificationEvent(entry, 'awaiting_show');
  } catch (error) {
    notificationEvent(entry, 'failed', 'desktop.notification_send_failed', error.stack || error.message || String(error));
    releaseNotification(entry, true);
  }
  return entry.last;
}
function installIPC() {
  const handle = (name, fn) => ipcMain.handle(name, async (event, ...args) => {
    const record = windows.get(BrowserWindow.fromWebContents(event.sender)?.id);
    if (!record || event.sender !== record.window.webContents || event.senderFrame !== event.sender.mainFrame || !local(event.senderFrame.url)) return { ok: false, error: transportError(new DesktopError('desktop.sender_untrusted', '此页面不能使用桌面接口。')) };
    try { return { ok: true, value: await fn(record, ...args) }; }
    catch (error) { return { ok: false, error: transportError(error) }; }
  });
  handle('desktop:state', record => ({ ...record.state })); handle('desktop:connect', connect);
  handle('desktop:configure', record => { disposePreview(record); record.remote?.setVisible(false); emit(record, { configuring: true }); });
  handle('desktop:resume', record => { if (record.scope && (!record.scope.closed || record.state.draftRecovery)) { record.remote?.setVisible(true); emit(record, { configuring: false }); } });
  handle('desktop:navigate', navigate);
  handle('desktop:action', (record, name) => { if (!['back', 'forward', 'reload', 'sidebar', 'search', 'theme', 'new-chat', 'settings', 'shortcuts'].includes(name)) throw new DesktopError('desktop.command_invalid', '此命令不可用。'); return executeCommand(name, record); });
  handle('desktop:locale', (_record, lang) => { if (!['zh', 'en'].includes(lang)) throw new DesktopError('desktop.locale_invalid', '语言无效。'); language = lang; installMenu(); });
  handle('desktop:window', (record, name) => { if (name === 'minimize') record.window.minimize(); else if (name === 'maximize') record.window.isMaximized() ? record.window.unmaximize() : record.window.maximize(); else if (name === 'close') record.window.close(); else throw new DesktopError('desktop.window_action_invalid', '窗口操作无效。'); });
  handle('desktop:new-window', (record, route) => createWindow(record.state.origin, safeRoute(route)).then(child => ({ windowId: child.window.id })));
  handle('desktop:external', openExternal); handle('desktop:preview', openPreview);
  handle('desktop:visualization-create', (record, input) => {
    if (!record.scope || record.scope.closed || !activeThread(record, input?.threadId) || typeof input.html !== 'string' || !input.html) throw new DesktopError('desktop.visualization_invalid', '交互图请求无效或所属会话已改变。');
    if (Buffer.byteLength(input.html, 'utf8') > 16 * 1024 * 1024) throw new DesktopError('desktop.visualization_too_large', '交互图超出桌面展示范围，请打开完整原文件。');
    const id = randomUUID(), host = `document-${id}`;
    documents.set(host, { id, record, scope: record.scope, threadId: input.threadId, html: input.html });
    return { id, url: `muteki-desktop://${host}/document.html` };
  });
  handle('desktop:visualization-release', (record, id) => {
    const host = `document-${id}`, document = documents.get(host);
    if (document?.record === record) documents.delete(host);
  });
  handle('desktop:preview-close', (record, input) => {
    const preview = record.preview;
    if (!preview || (input?.id && preview.id !== input.id) || (input?.surfaceId && preview.surfaceId !== input.surfaceId)) return;
    if (input?.hide) { preview.setVisible(false); preview.hidden = true; record.window.webContents.focus(); } else disposePreview(record);
  });
  handle('desktop:preview-action', (record, input) => {
    const preview = record.preview;
    if (!preview || preview.scope !== record.scope || preview.scope.closed || !activeThread(record, preview.threadId) || preview.id !== input?.id) throw new DesktopError('desktop.preview_changed', '预览已改变。');
    const contents = preview.webContents;
    if (input.action === 'back' && contents.navigationHistory.canGoBack()) contents.navigationHistory.goBack();
    else if (input.action === 'forward' && contents.navigationHistory.canGoForward()) contents.navigationHistory.goForward();
    else if (input.action === 'reload') contents.reload();
    else if (input.action === 'stop') contents.stop();
    else throw new DesktopError('desktop.preview_action_unavailable', '此预览操作不可用。');
  });
  handle('desktop:close-ack', async (record, input) => {
    const barrier = record.barriers.get(input?.token);
    if (barrier) { clearTimeout(barrier.timer); record.barriers.delete(input.token); barrier.resolve({ persisted: input.persisted === true, error: input.error }); return; }
    if (!input || input.token !== record.pendingClose) throw new DesktopError('desktop.close_ack_stale', '关闭请求已改变。');
    clearTimeout(record.closeTimer); record.pendingClose = undefined;
    if (!input.persisted) { const { response } = await dialog.showMessageBox(record.window, { type: 'warning', message: text('草稿保存失败。仍然关闭窗口？', 'Draft saving failed. Close the window anyway?'), detail: String(input.error || ''), buttons: [text('保持打开', 'Keep open'), text('关闭', 'Close')], defaultId: 0, cancelId: 0 }); if (response !== 1) { quitting = false; return; } }
    persist(record); record.allowClose = true; record.window.close();
  });
  handle('desktop:attachment-cache', (record, input) => {
    if (!(input?.data instanceof ArrayBuffer) || input.data.byteLength > 256 * 1024 * 1024) throw new DesktopError('desktop.attachment_cache_limit', '附件超过桌面缓存范围。');
    const scope = record.scope;
    const operation = cacheWrite.then(() => { if (scope !== record.scope) throw new DesktopError('desktop.connection_changed', '服务连接已改变。'); return saveAttachment(cacheRoot, scope, input); });
    cacheWrite = operation.catch(() => {}); return operation;
  });
  handle('desktop:attachment-restore', (record, input) => restoreAttachment(cacheRoot, record.scope, input));
  handle('desktop:attachment-remove', (record, input) => removeAttachment(cacheRoot, record.scope, input));
  handle('desktop:attachment-cache-usage', () => cacheUsage(cacheRoot));
  handle('desktop:select-path', async (record, input) => {
    if (!input || !['directory', 'file'].includes(input.kind)) throw new DesktopError('desktop.path_kind_invalid', '请选择文件或目录。');
    const result = await dialog.showOpenDialog(record.window, { properties: [input.kind === 'directory' ? 'openDirectory' : 'openFile'], title: text('选择桌面客户端路径', 'Select a desktop client path') });
    if (result.canceled) return null;
    const selected = await fs.promises.realpath(result.filePaths[0]), id = randomUUID(); record.selectedPaths.set(id, selected);
    return { id, path: selected, name: path.basename(selected), host: 'desktop-client', serverMapped: false };
  });
  handle('desktop:open-path', async (record, input) => {
    const selected = record.selectedPaths.get(input?.id);
    if (!selected || !['reveal', 'open'].includes(input.action)) throw new DesktopError('desktop.path_not_selected', '先通过桌面选择器选择该路径。');
    const resolved = await fs.promises.realpath(selected);
    if (resolved !== selected) throw new DesktopError('desktop.path_changed', '选择的路径已改变，请重新选择。');
    if (input.action === 'reveal') shell.showItemInFolder(resolved);
    else { const error = await shell.openPath(resolved); if (error) throw new DesktopError('desktop.path_open_failed', error); }
  });
  handle('desktop:workspace-root', async (record, input) => {
    const scope = record.scope;
    if (!scope?.serviceId || scope.closed || !activeThread(record, input?.threadId) || input?.serviceId !== scope.serviceId || input?.identityId !== scope.identityId || !input.workspaceId || !input.serviceRoot) throw new DesktopError('desktop.workspace_scope_changed', '当前工作台或工作区归属已改变。');
    const result = await dialog.showOpenDialog(record.window, { properties: ['openDirectory'], title: text('选择此工作区对应的本机目录', 'Select the corresponding local workspace directory'), message: text('这是手工映射。服务端路径与本机路径分别归属，文件同步尚未验证。', 'This is a manual mapping. Service and client paths have separate hosts; file synchronization is unverified.') });
    if (result.canceled) return null;
    if (scope !== record.scope || scope.closed || !activeThread(record, input.threadId)) throw new DesktopError('desktop.connection_changed', '服务连接或会话已改变。');
    const clientRoot = await fs.promises.realpath(result.filePaths[0]), grantId = randomUUID();
    if (scope !== record.scope || scope.closed || !activeThread(record, input.threadId)) throw new DesktopError('desktop.connection_changed', '服务连接或会话已改变。');
    const grant = { grantId, threadId: input.threadId, workspaceId: input.workspaceId, serviceId: scope.serviceId, identityId: scope.identityId, serviceRoot: input.serviceRoot, clientRoot, host: 'desktop-client', mapping: 'user-selected', scope };
    record.workspaceGrants.set(grantId, grant);
    const { scope: _scope, ...publicGrant } = grant; return publicGrant;
  });
  handle('desktop:workspace-file', async (record, input) => {
    const grant = record.workspaceGrants.get(input?.grantId), scope = record.scope;
    if (!grant || grant.scope !== scope || scope.closed || !activeThread(record, grant.threadId) || ['threadId', 'workspaceId', 'serviceId', 'identityId'].some(key => input[key] !== grant[key])) throw new DesktopError('desktop.workspace_scope_changed', '本机目录映射或会话已改变，请重新选择。');
    if (typeof input.relativePath !== 'string' || !input.relativePath || path.isAbsolute(input.relativePath) || !['reveal', 'open'].includes(input.action)) throw new DesktopError('desktop.workspace_file_invalid', '请选择工作区内的相对文件路径。');
    const root = await fs.promises.realpath(grant.clientRoot), file = await fs.promises.realpath(path.resolve(root, input.relativePath));
    if (root !== grant.clientRoot || !file.startsWith(root + path.sep)) throw new DesktopError('desktop.workspace_file_outside', '文件不属于已选择的本机工作区。');
    if (scope !== record.scope || scope.closed || !activeThread(record, grant.threadId)) throw new DesktopError('desktop.connection_changed', '服务连接或会话已改变。');
    if (input.action === 'reveal') shell.showItemInFolder(file);
    else { const error = await shell.openPath(file); if (error) throw new DesktopError('desktop.workspace_file_open_failed', error); }
  });
  handle('desktop:microphone', async record => {
    if (process.platform !== 'darwin') return { granted: false, code: 'desktop.microphone_os_settings', settings: 'system' };
    const scope = record.scope;
    const granted = await systemPreferences.askForMediaAccess('microphone');
    if (scope !== record.scope || scope?.closed) throw new DesktopError('desktop.connection_changed', '服务连接已经改变，请在当前工作台重新开始语音输入。');
    record.microphoneGranted = granted;
    return { granted, status: systemPreferences.getMediaAccessStatus('microphone') };
  });
  handle('desktop:speech-start', (record, input) => nativeSpeech.start(record, input));
  handle('desktop:speech-finish', (record, input) => nativeSpeech.finish(record, input));
  handle('desktop:speech-cancel', (record, input) => nativeSpeech.cancel(record, input));
  handle('desktop:notification-status', notificationStatus);
  handle('desktop:notification-request', requestNotifications);
  handle('desktop:notification-send', sendNotification);
  handle('desktop:permission-settings', async (_record, name) => {
    if (!['microphone', 'speechRecognition', 'notifications'].includes(name)) throw new DesktopError('desktop.permission_invalid', '权限设置类型无效。');
    if (process.platform === 'darwin') await shell.openExternal(name === 'microphone' ? 'x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone' : name === 'speechRecognition' ? 'x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition' : 'x-apple.systempreferences:com.apple.Notifications-Settings.extension');
    else if (process.platform === 'win32') await shell.openExternal(name === 'microphone' ? 'ms-settings:privacy-microphone' : 'ms-settings:notifications');
    else throw new DesktopError('desktop.permission_settings_manual', '请在系统设置中管理 Muteki 的麦克风或通知权限。');
  });
  handle('desktop:terminal-open', (record, input) => {
    if (!record.scope || record.scope.closed || !input || typeof input.path !== 'string' || typeof input.threadId !== 'string' || !activeThread(record, input.threadId)) throw new DesktopError('desktop.terminal_invalid', '终端请求无效或所属会话已改变。');
    const url = new URL(input.path, record.scope.origin);
    const segments = url.pathname.split('/');
    if (url.origin !== record.scope.origin || segments.length !== 9 || segments[1] !== 'api' || segments[2] !== 'threads' || segments[3] !== encodeURIComponent(input.threadId) || segments[4] !== 'workspace' || segments[5] !== 'terminal' || segments[6] !== 'sessions' || !segments[7] || segments[8] !== 'stream' || [...url.searchParams.keys()].some(key => key !== 'ticket')) throw new DesktopError('desktop.terminal_scope_invalid', '终端必须属于当前工作台会话。');
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    const id = randomUUID(), socket = new WebSocket(url), scope = record.scope; record.terminals.set(id, socket);
    socket.threadId = input.threadId; socket.scope = scope;
    const send = value => { if (record.scope === scope && !record.window.isDestroyed()) record.window.webContents.send('desktop:terminal', { id, ...value }); };
    socket.addEventListener('open', () => send({ type: 'open' }));
    socket.addEventListener('message', event => send({ type: 'message', data: typeof event.data === 'string' ? event.data : '' }));
    socket.addEventListener('error', () => send({ type: 'error', code: 'desktop.terminal_connection_failed', message: '终端连接失败。' }));
    socket.addEventListener('close', event => { record.terminals.delete(id); send({ type: 'close', code: event.code, message: event.reason }); });
    return { id };
  });
  handle('desktop:terminal-send', (record, id, data) => { const socket = record.terminals.get(id); if (!socket || socket.scope !== record.scope || socket.scope.closed || !activeThread(record, socket.threadId) || socket.readyState !== WebSocket.OPEN || typeof data !== 'string') throw new DesktopError('desktop.terminal_not_open', '终端尚未连接到当前会话。', { retryable: true }); socket.send(data); });
  handle('desktop:terminal-close', (record, id) => { record.terminals.get(id)?.close(); record.terminals.delete(id); });
}
function installMenu() {
  const role = (name, zh, en) => ({ role: name, label: text(zh, en) });
  const item = (zh, en, name, accelerator) => ({ label: text(zh, en), accelerator, click: () => void executeCommand(name).catch(error => { const record = focused(); if (record) emit(record, { message: error.message }); }) });
  Menu.setApplicationMenu(Menu.buildFromTemplate([
    ...(process.platform === 'darwin' ? [{ label: 'Muteki', submenu: [role('about', '关于 Muteki', 'About Muteki'), { type: 'separator' }, item('设置…', 'Settings…', 'settings', 'CmdOrCtrl+,'), { label: text('服务连接…', 'Service connection…'), click: () => { const record = focused(); if (record) emit(record, { configuring: true }); } }, { type: 'separator' }, role('hide', '隐藏 Muteki', 'Hide Muteki'), role('hideOthers', '隐藏其他应用', 'Hide others'), role('unhide', '显示全部', 'Show all'), { type: 'separator' }, role('quit', '退出 Muteki', 'Quit Muteki')] }] : []),
    { label: text('对话', 'Chat'), submenu: [item('新建对话', 'New chat', 'new-chat', 'CmdOrCtrl+Shift+N'), item('搜索对话', 'Search conversations', 'search', 'CmdOrCtrl+K'), { label: text('新建窗口', 'New window'), click: () => void createWindow(focused()?.state.origin).catch(error => dialog.showErrorBox('Muteki', error.message)) }, { type: 'separator' }, role('close', '关闭窗口', 'Close window')] },
    { label: text('编辑', 'Edit'), submenu: [role('undo', '撤销', 'Undo'), role('redo', '重做', 'Redo'), { type: 'separator' }, role('cut', '剪切', 'Cut'), role('copy', '拷贝', 'Copy'), role('paste', '粘贴', 'Paste'), role('selectAll', '全选', 'Select all')] },
    { label: text('视图', 'View'), submenu: [item('后退', 'Back', 'back', 'Alt+Left'), item('前进', 'Forward', 'forward', 'Alt+Right'), item('刷新', 'Reload', 'reload', 'CmdOrCtrl+R'), item('切换会话侧栏', 'Toggle chat sidebar', 'sidebar', 'CmdOrCtrl+B'), item('快捷键', 'Keyboard shortcuts', 'shortcuts'), { type: 'separator' }, role('resetZoom', '实际大小', 'Actual size'), role('zoomIn', '放大', 'Zoom in'), role('zoomOut', '缩小', 'Zoom out'), role('togglefullscreen', '切换全屏', 'Toggle fullscreen')] }, role('windowMenu', '窗口', 'Window'),
  ]));
}
async function deepLink(value) {
  let url;
  try { url = new URL(value); } catch { throw new DesktopError('desktop.deep_link_invalid', '聊天链接无效。'); }
  if (url.protocol !== 'muteki:' || url.hostname !== 'chat' || url.username || url.password || url.hash || !/^\/[^/]+$/.test(url.pathname) || [...url.searchParams.keys()].some(key => key !== 'service')) throw new DesktopError('desktop.deep_link_invalid', '聊天链接格式无效。');
  const origin = normalizeOrigin(url.searchParams.get('service')), route = `/chat${url.pathname}`;
  const record = focused();
  if (record?.state.origin === origin) return navigate(record, route);
  if (record) { const { response } = await dialog.showMessageBox(record.window, { type: 'question', message: text('打开另一个工作台的聊天？', 'Open a chat in another workspace?'), detail: `${origin}${route}`, buttons: [text('取消', 'Cancel'), text('打开新窗口', 'Open new window')], defaultId: 0, cancelId: 0 }); if (response !== 1) return; }
  await createWindow(origin, route);
}
const pendingLinks = process.argv.filter(value => value.startsWith('muteki://'));
app.on('open-url', (event, url) => { event.preventDefault(); if (app.isReady()) void deepLink(url).catch(error => dialog.showErrorBox('Muteki', error.message)); else pendingLinks.push(url); });
if (!app.requestSingleInstanceLock()) app.quit();
else {
  app.on('second-instance', (_event, argv) => { const link = argv.find(value => value.startsWith('muteki://')); if (link) void deepLink(link).catch(error => dialog.showErrorBox('Muteki', error.message)); else { const record = focused(); if (record) { record.window.restore(); record.window.show(); record.window.focus(); } } });
  app.on('window-all-closed', () => { if (process.platform !== 'darwin') app.quit(); });
  app.on('before-quit', event => { if (windows.size) { event.preventDefault(); quitting = true; for (const record of windows.values()) record.window.close(); } });
  app.on('activate', () => { if (!windows.size) void createWindow().catch(error => dialog.showErrorBox('Muteki 无法打开窗口', error.message)); });
  app.whenReady().then(async () => {
    const root = path.resolve(__dirname, '../renderer-dist');
    const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png', '.svg': 'image/svg+xml', '.woff2': 'font/woff2', '.ico': 'image/x-icon' };
    protocol.handle('muteki-desktop', async request => {
      const url = new URL(request.url);
      const document = documents.get(url.host);
      if (document && document.scope === document.record.scope && !document.scope.closed && request.method === 'GET' && url.pathname === '/document.html') {
        return new Response(document.html, { headers: { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff',
          'Content-Security-Policy': "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline' https://cdnjs.cloudflare.com https://esm.sh https://cdn.jsdelivr.net https://unpkg.com; style-src 'unsafe-inline' https://cdnjs.cloudflare.com https://esm.sh https://cdn.jsdelivr.net https://unpkg.com https://fonts.googleapis.com https://fonts.bunny.net; font-src https://fonts.gstatic.com https://fonts.bunny.net; img-src data: blob: https://cdnjs.cloudflare.com https://esm.sh https://cdn.jsdelivr.net https://unpkg.com; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors muteki-desktop://app" } });
      }
      if (url.host !== 'app') { const entry = scopes.get(url.host); return forwardService(request, entry?.scope, (scope, data) => identity(entry.record, scope, data)); }
      try {
        const pathname = decodeURIComponent(url.pathname);
        const asset = pathname === '/' || /^\/(chat|settings)(\/|$)/.test(pathname) ? '/index.html' : pathname;
        const file = path.resolve(root, `.${asset}`), type = types[path.extname(file)];
        if (!file.startsWith(`${root}${path.sep}`) || !type) return new Response('Not found', { status: 404 });
        return new Response(await fs.promises.readFile(file), { headers: { 'Content-Type': type, 'Content-Security-Policy': "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src muteki-desktop:; frame-src muteki-desktop: blob:; base-uri 'self'; form-action 'none'; frame-ancestors 'none'", 'X-Content-Type-Options': 'nosniff' } });
      } catch { return new Response('Not found', { status: 404 }); }
    });
    session.defaultSession.setPermissionCheckHandler((contents, permission, requestingOrigin, details) => {
      const record = contents && windows.get(BrowserWindow.fromWebContents(contents)?.id);
      return Boolean(record && contents === record.window.webContents && local(contents.getURL()) && local(requestingOrigin) && details?.isMainFrame !== false && (permission === 'clipboard-sanitized-write' || (permission === 'notifications' && notificationsAllowed(record)) || (permission === 'media' && details?.mediaType === 'audio' && record.microphoneGranted)));
    });
    session.defaultSession.setPermissionRequestHandler(async (contents, permission, callback, details) => {
      const record = windows.get(BrowserWindow.fromWebContents(contents)?.id);
      if (!record || contents !== record.window.webContents || !local(contents.getURL()) || details?.isMainFrame === false) return callback(false);
      if (permission === 'notifications') {
        // Chromium checks can return denied before requestPermission is reached.
        // Only the explicit, scope-bound desktop request above asks the user.
        callback(notificationsAllowed(record));
        return;
      }
      callback(permission === 'clipboard-sanitized-write' || (permission === 'media' && record.microphoneGranted && details?.mediaTypes?.length > 0 && details?.mediaTypes?.every(type => type === 'audio')));
    });
    session.defaultSession.on('will-download', (_event, item) => item.setSaveDialogOptions({
      title: text('保存 Muteki 文件', 'Save Muteki file'),
      defaultPath: path.join(app.getPath('downloads'), path.basename(item.getFilename())),
    }));
    if (app.isPackaged && process.env.MUTEKI_DESKTOP_REGISTER_PROTOCOL !== '0') app.setAsDefaultProtocolClient('muteki');
    installIPC(); installMenu();
    if (pendingLinks.length) { for (const link of pendingLinks) await deepLink(link); }
    else await createWindow(process.env.MUTEKI_DESKTOP_URL || '');
  }).catch(error => { dialog.showErrorBox('Muteki 无法启动', error.message || '请构建桌面界面后重试。'); for (const record of windows.values()) { record.allowClose = true; record.window.destroy(); } });
}
