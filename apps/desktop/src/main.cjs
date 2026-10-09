const { app, BrowserWindow, WebContentsView, Menu, Notification: NativeNotification, dialog, ipcMain, protocol, session, shell, screen, systemPreferences, safeStorage } = require('electron');
const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { randomUUID, createHash } = require('node:crypto');
const { SHELL_URL, normalizeOrigin, normalizeDesktopRoute, desktopNavigationTarget, parseWebUrl, partitionFor, webPreferences } = require('./policy.cjs');
const { readPreferences, writePreferences, notificationConsentKey } = require('./preferences.cjs');
const { DesktopError, transportError, errorResponse, probeService, forwardService, closeScope } = require('./transport.cjs');
const { saveAttachment, restoreAttachment, removeAttachment, cacheUsage } = require('./attachment-cache.cjs');
const { NativeSpeech } = require('./speech.cjs');
const { ServiceAuthSessions } = require('./auth.cjs');
let authSessions;
const { createPreviewControl, pickElement, cancelPick } = require('./preview-control.cjs');
const { desktopEnvironment, runtimePaths, atomicJson } = require('./environment.cjs');
const { ManagedService } = require('./managed-service.cjs');

const launchOption = name => process.argv.find(value => value.startsWith(`--muteki-${name}=`))?.split('=').slice(1).join('=');
const workspace = process.env.MUTEKI_DESKTOP_WORKSPACE || path.resolve(__dirname, '../../..');
const buildChannel = require(path.join(app.getAppPath(), 'package.json')).mutekiChannel;
if (app.isPackaged && buildChannel !== 'dev' && launchOption('channel') === 'dev') throw new Error('Use the dedicated Muteki Dev build for desktop automation');
const environment = desktopEnvironment({ appData: process.env.MUTEKI_DESKTOP_USER_DATA || app.getPath('appData'), packaged: app.isPackaged,
  channel: launchOption('channel') || buildChannel,
  root: launchOption('environment-root') || process.env.MUTEKI_ENVIRONMENT_ROOT || (process.env.MUTEKI_DESKTOP_USER_DATA ? path.join(process.env.MUTEKI_DESKTOP_USER_DATA, 'environment') : undefined), workspace });
app.setName(environment.name);
app.setPath('userData', process.env.MUTEKI_DESKTOP_USER_DATA ? path.resolve(process.env.MUTEKI_DESKTOP_USER_DATA) : environment.paths.desktop);
const selfCheck = process.argv.includes('--muteki-self-check');
if (selfCheck && environment.channel !== 'candidate') throw new Error('Candidate self-check requires an isolated candidate environment');
let updateJournal = launchOption('update-journal');
const {unfinishedUpdate, helperRunning, atomicJson: writeUpdateJournal} = require('./update-files.cjs');
const interruptedUpdate = app.isPackaged && environment.channel === 'stable' && !updateJournal ? unfinishedUpdate(environment.root) : null;
const repairingUpdate = interruptedUpdate?.journal.activatedAt && !helperRunning(interruptedUpdate.journal);
if (repairingUpdate) {
  updateJournal = interruptedUpdate.file;
  writeUpdateJournal(updateJournal, {...interruptedUpdate.journal, phase: 'validating'});
}
if (updateJournal) {
  const allowed = path.join(environment.root, 'updates') + path.sep;
  if (!path.resolve(updateJournal).startsWith(allowed) || environment.channel !== 'stable') throw new Error('Invalid update recovery journal');
  const journal = JSON.parse(fs.readFileSync(updateJournal, 'utf8'));
  if (journal.environmentRoot !== environment.root || journal.phase !== 'validating') throw new Error('Update recovery context does not match this environment');
  environment.maintenance = true;
}
const patchUpdate = patch => writeUpdateJournal(updateJournal, { ...JSON.parse(fs.readFileSync(updateJournal, 'utf8')), ...patch });
let desktopUpdates, applyUpdate = false, quitApproved = false;
let managedService;
function updatesController() {
  if (!desktopUpdates) {
    const {DesktopUpdates} = require('./updates.cjs');
    desktopUpdates = new DesktopUpdates(app, environment, status => {
      for (const record of windows.values()) if (!record.window.isDestroyed()) record.window.webContents.send('desktop:update-status', status);
    });
  }
  return desktopUpdates;
}
function managedRuntime() {
  if (!managedService) {
    managedService = new ManagedService(environment, runtimePaths({ packaged: app.isPackaged && buildChannel !== 'dev', resources: process.resourcesPath, workspace }));
    managedService.on('state', state => {
      for (const record of windows.values()) {
        const failed = state.state === 'failed' && record.scope?.environmentId === environment.id;
        if (failed) closeScope(record.scope);
        emit(record, { managedService: state, ...(failed ? { status: 'error', configuring: true, message: state.error } : {}) });
      }
    });
  }
  return managedService;
}
app.enableSandbox();
protocol.registerSchemesAsPrivileged([{ scheme: 'muteki-desktop', privileges: { standard: true, secure: true, supportFetchAPI: true, stream: true, corsEnabled: true } }]);
const preferencesFile = path.join(app.getPath('userData'), 'connection.json');
let preferences = readPreferences(preferencesFile), language = 'zh', quitting = false;
const windows = new Map(), scopes = new Map(), documents = new Map();
const deliveredNotificationEvents = new Map();
const requestedNotificationSounds = new Set();
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
function isNotificationOwner(record) {
  const scope = record.scope;
  if (!scope || scope.closed || !scope.serviceId || !scope.identityId || record.window.isDestroyed()) return false;
  const key = notificationConsentKey(scope);
  return [...windows.values()].find(owner => owner.scope && !owner.scope.closed && !owner.window.isDestroyed()
    && notificationConsentKey(owner.scope) === key) === record;
}
function notificationWindowFocused(record) {
  return (process.platform !== 'darwin' || app.isActive()) && record.window.isFocused();
}
function refreshNotificationOwners() {
  for (const owner of windows.values()) {
    const enabled = isNotificationOwner(owner);
    const workspaceKey = owner.scope && !owner.scope.closed ? notificationConsentKey(owner.scope) : '';
    const visibleThreads = workspaceKey ? [...windows.values()].filter(other => other.scope && !other.scope.closed && !other.window.isDestroyed()
      && notificationConsentKey(other.scope) === workspaceKey && !other.state.configuring && !other.remote && other.window.isVisible() && !other.window.isMinimized() && notificationWindowFocused(other)).flatMap(other => {
        try { const route = new URL(other.state.route, SHELL_URL); return route.pathname.startsWith('/chat/') ? [decodeURIComponent(route.pathname.split('/')[2] || '')] : []; } catch { return []; }
      }) : [];
    if (owner.state.notificationOwner === enabled && owner.state.notificationWorkspaceKey === workspaceKey
      && JSON.stringify(owner.state.notificationVisibleThreadIds) === JSON.stringify(visibleThreads)) continue;
    if (!enabled && owner.state.notificationOwner) disposeNotifications(owner, 'desktop.notification_owner_changed');
    owner.state.notificationWorkspaceKey = workspaceKey;
    owner.state.notificationOwner = enabled;
    owner.state.notificationVisibleThreadIds = visibleThreads;
    if (!owner.window.isDestroyed()) owner.window.webContents.send('desktop:state-changed', { ...owner.state });
  }
}
function emit(record, patch = {}) {
  if (patch.route && patch.route !== record.state.route) nativeSpeech.cancel(record);
  if (patch.message || patch.persistenceWarning) patch.noticeVersion = (record.state.noticeVersion || 0) + 1;
  Object.assign(record.state, patch);
  refreshNotificationOwners();
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
  route = safeRoute(route);
  record.navigation = { entries: [route], index: 0 };
  return { route, canGoBack: false, canGoForward: false };
}
function commitRoute(record, route, mode, historyIndex) {
  route = safeRoute(route);
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
  if (name === 'reload') emit(record, {anchorTarget: undefined});
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
function handoffNotificationEntries(record) {
  for (const entry of [...(record.notifications?.values() || [])]) {
    const key = notificationConsentKey(entry.scope);
    const next = [...windows.values()].find(owner => owner !== record && owner.scope && !owner.scope.closed
      && !owner.window.isDestroyed() && notificationConsentKey(owner.scope) === key && isNotificationOwner(owner));
    if (!next || !notificationsAllowed(next)) continue;
    next.notifications ||= new Map();
    if (next.notifications.has(entry.threadId)) continue;
    record.notifications.delete(entry.threadId);
    entry.record = next; entry.scope = next.scope;
    if (entry.last) entry.last = { ...entry.last, connectionVersion: next.scope.version, serviceId: next.scope.serviceId,
      identityId: next.scope.identityId, seq: next.notificationSequence = (next.notificationSequence || 0) + 1 };
    next.notifications.set(entry.threadId, entry);
  }
}
function disposeConnection(record) {
  nativeSpeech.cancel(record);
  closeScope(record.scope);
  handoffNotificationEntries(record);
  disposeNotifications(record, 'desktop.notification_scope_closed');
  if (record.scope) scopes.delete(record.scope.host);
  record.scope = undefined;
  for (const socket of record.terminals.values()) socket.close();
  record.terminals.clear(); disposePreview(record); disposeRemote(record);
  record.workspaceGrants.clear();
  record.notificationPermission = undefined;
  record.notificationDiagnostic = undefined;
  record.microphoneGranted = false;
  refreshNotificationOwners();
  for (const [host, document] of documents) if (document.record === record) documents.delete(host);
}
function persist(record) {
  try {
    if (!record.state.origin) return;
    preferences = { ...preferences, origin: record.state.origin, mode: record.state.connectionMode || preferences.mode || 'external', bounds: record.window.getNormalBounds() };
    if (record.scope?.serviceId && record.scope.identityId) {
      const key = `${record.scope.environmentId ? 'managed:' + record.scope.environmentId : record.scope.origin}|${record.scope.serviceId}|${record.scope.identityId}`;
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
    const service = await probeService(origin, controller.signal);
    const managed = managedService?.ready?.origin === origin ? managedService.ready : null;
    if (managed && (service.health.desktop_runtime?.environment_id !== environment.id || service.health.desktop_runtime?.generation !== environment.generation || service.auth.service_id !== managed.service_id)) throw new DesktopError('desktop.managed_identity_mismatch', '本机工作台身份校验失败。');
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
    const scope = { origin, host, version: attempt, requests: new Set(), closed: false, auth: authSessions.forService(origin, service.auth.service_id) };
    if (managed) {
      scope.environmentId = environment.id;
      scope.auth.accept({ ...managed, auth_required: true, session_protocol: 2, session_owner: 'desktop' }, false, scope);
    }
    record.scope = scope; scopes.set(host, { record, scope });
    record.navigationGeneration = (record.navigationGeneration || 0) + 1;
    record.routeRequested = false;
    emit(record, { origin, connectionMode: managed ? 'local' : 'external', transportOrigin: `muteki-desktop://${host}`, connectionVersion: attempt,
      status: 'connected', configuring: false, serviceVersion: service.health.version, serviceFeatures: service.health.features, anchorTarget: undefined, remoteUiBuild: undefined, ...resetNavigation(record, '/chat'), serviceId: '', identityId: '', message: '', candidateOrigin: '', draftRecovery: false });
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
async function connectLocal(record) {
  if (record.connecting) throw new DesktopError('desktop.connection_busy', '正在连接，请稍候。');
  emit(record, { configuring: true, status: 'connecting', message: '' });
  try {
    await managedRuntime().start();
    const runtime = await managedRuntime().refreshSession();
    if (record.window.isDestroyed()) return;
    return await connect(record, runtime.origin);
  } catch (error) {
    if (!record.window.isDestroyed()) emit(record, { status: 'error', configuring: true, message: error.message });
    throw error;
  }
}
async function renewManagedSessions() {
  if (!managedService?.ready || environment.maintenance) return;
  const records = [...windows.values()].filter(record => record.scope?.environmentId === environment.id && record.scope.auth.token && record.scope.auth.expiresAt < Date.now() / 1000 + 600);
  if (!records.length) return;
  const generation = environment.generation;
  const runtime = await managedService.refreshSession();
  for (const record of records) {
    const scope = record.scope;
    if (generation !== environment.generation || scope?.environmentId !== environment.id || scope.closed || !scope.auth.token) continue;
    scope.auth.accept({...runtime, auth_required: true, session_protocol: 2, session_owner: 'desktop'}, false, scope);
    if (record.remote && !record.remote.webContents.isDestroyed()) await record.remote.webContents.session.cookies.set({url: runtime.origin + '/api', name: runtime.cookie_name, value: runtime.token, httpOnly: true, sameSite: 'strict', path: '/api'});
  }
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
// Must match the `.service-surface` card geometry in renderer/styles.css so the
// native page sits exactly on the raised card and its CSS shadow shows around it.
const SURFACE_INSET = { left: 56, top: 46, right: 8, bottom: 8 }, SURFACE_RADIUS = 12;
// The desktop activity rail replaces the web app's own rail on remote pages,
// which also hides that rail's theme toggle; applyRemoteAppearance takes over.
const REMOTE_CHROME_CSS = '.workspace-frame { --workspace-rail-width: 0px; grid-template-columns: minmax(0, 1fr) !important; } .workspace-activity-rail { display: none !important; }';
async function applyRemoteAppearance(record, contents = record.remote?.webContents) {
  const appearance = record.appearance;
  if (!appearance || !contents || contents.isDestroyed()) return;
  const payload = {...appearance, serviceId: record.scope?.serviceId || '', identityId: record.scope?.identityId || ''};
  const script = `(() => {
    const value = ${JSON.stringify(payload)}, key = 'muteki.desktop.appearanceOverride';
    const serialized = JSON.stringify(value);
    if (localStorage.getItem(key) !== serialized) {
      localStorage.setItem(key, serialized);
      window.dispatchEvent(new StorageEvent('storage', {key, newValue: serialized, storageArea: localStorage}));
    }
    const root = document.documentElement;
    root.dataset.theme = value.resolvedTheme;
    root.classList.toggle('dark', value.resolvedTheme === 'dark'); root.classList.toggle('light', value.resolvedTheme === 'light');
    const result = {build: root.dataset.mutekiUiBuild || '', appearanceContract: root.dataset.mutekiAppearanceContract || ''};
    if (result.appearanceContract !== '1' || root.dataset.desktopAppearanceApplied === serialized) return result;
    return new Promise((resolve, reject) => {
      const observer = new MutationObserver(() => {
        if (root.dataset.desktopAppearanceApplied === serialized) { observer.disconnect(); clearTimeout(timeout); resolve(result); }
      });
      const timeout = setTimeout(() => { observer.disconnect(); reject(new Error('远程界面未确认外观已应用，请更新或重新加载 Web 界面。')); }, 5000);
      observer.observe(root, {attributes: true, attributeFilter: ['data-desktop-appearance-applied']});
    });
  })()`;
  const info = await contents.executeJavaScript(script);
  if (record.remote?.webContents === contents && !contents.isDestroyed()) {
    emit(record, {remoteUiBuild: info.build || 'unknown', remoteAppearanceContract: info.appearanceContract});
  }
}
async function applyRemoteChrome(record, contents) {
  if (contents.isDestroyed()) return;
  await contents.insertCSS(REMOTE_CHROME_CSS);
  await applyRemoteAppearance(record, contents);
}
function parseAppearance(input) {
  const selection = input?.selection;
  const valid = input && ['system', 'light', 'dark'].includes(input.preference) && ['light', 'dark'].includes(input.resolvedTheme) && selection && (
    (selection.kind === 'preset' && ['azure', 'violet', 'teal', 'ember'].includes(selection.id))
    || (selection.kind === 'custom' && Number.isFinite(selection.hue) && selection.hue >= 0 && selection.hue < 360));
  if (!valid) throw new DesktopError('desktop.appearance_invalid', '外观设置无效。');
  return {preference: input.preference, resolvedTheme: input.resolvedTheme, selection: selection.kind === 'custom' ? {kind: 'custom', hue: selection.hue} : {kind: 'preset', id: selection.id}};
}

function remoteBounds(record) {
  if (!record.remote || record.window.isDestroyed()) return;
  const [width, height] = record.window.getContentSize(), { left, top, right, bottom } = SURFACE_INSET;
  record.remote.setBounds({ x: left, y: top, width: Math.max(0, width - left - right), height: Math.max(0, height - top - bottom) });
}
async function navigate(record, value, mode = 'push', historyIndex) {
  const target = desktopNavigationTarget(value);
  const route = target.route === '/' ? '/chat' : target.route;
  const anchorTarget = target.hash ? {hash: target.hash, id: (record.anchorSequence = (record.anchorSequence || 0) + 1)} : undefined;
  if (!['push', 'replace', 'traverse'].includes(mode) || (mode === 'traverse' && (!Number.isInteger(historyIndex) || record.navigation?.entries[historyIndex] !== route))) throw new DesktopError('desktop.navigation_invalid', '导航历史请求无效。');
  record.routeRequested = true;
  const remoteRoute = !/^\/(chat|settings)(\/|\?|$)/.test(route) && route !== '/';
  if (route === record.state.route && !record.state.configuring && !record.navigationPending
      && (!remoteRoute || (record.remote && !record.remote.webContents.isDestroyed()))) {
    if (mode === 'traverse' && historyIndex !== record.navigation?.index) { commitRoute(record, route, mode, historyIndex); persist(record); }
    if (!remoteRoute) emit(record, {anchorTarget});
    else if (target.hash) await record.remote.webContents.loadURL(record.scope.origin + route + target.hash);
    return;
  }
  const generation = record.navigationGeneration = (record.navigationGeneration || 0) + 1;
  record.navigationPending = false;
  disposePreview(record);
  emit(record, {anchorTarget: undefined});
  if (/^\/(chat|settings)(\/|\?|$)/.test(route) || route === '/') {
    disposeRemote(record); commitRoute(record, route, mode, historyIndex);
    emit(record, {anchorTarget});
    persist(record); return;
  }
  if (!record.scope) throw new DesktopError('desktop.not_connected', '请先连接工作台。');
  disposeRemote(record);
  let remote;
  try {
    const origin = record.scope.origin, partition = partitionFor(origin, record.scope.environmentId);
    const remoteSession = session.fromPartition(partition);
    if (record.scope.environmentId && managedService?.ready && record.scope.auth.token) {
      await remoteSession.cookies.set({ url: origin + '/api', name: managedService.ready.cookie_name, value: record.scope.auth.token, httpOnly: true, sameSite: 'strict', path: '/api' });
    }
    const permitsClipboard = (contents, permission, requestingOrigin, details) =>
      permission === 'clipboard-sanitized-write' && details?.isMainFrame !== false &&
      parseWebUrl(requestingOrigin)?.origin === origin && parseWebUrl(contents?.getURL())?.origin === origin &&
      Array.from(windows.values()).some(owner => owner.remote?.webContents === contents && owner.scope?.origin === origin && !owner.scope.closed);
    remoteSession.setPermissionCheckHandler(permitsClipboard);
    remoteSession.setPermissionRequestHandler((contents, permission, callback, details) =>
      callback(permitsClipboard(contents, permission, details?.requestingUrl, details)));
    remote = new WebContentsView({ webPreferences: webPreferences(partition) });
    remote.setBorderRadius(SURFACE_RADIUS);
    remote.setVisible(false);
    remote.webContents.setUserAgent(remote.webContents.getUserAgent().replace(/\sMutekiDesktop\/[^\s]+/g, '') + ` MutekiDesktop/${app.getVersion()}`);
    remote.webContents.on('dom-ready', () => { if (record.remote === remote) void applyRemoteChrome(record, remote.webContents).catch(error => { if (record.remote === remote) emit(record, {message: `远程界面外观同步失败：${error.message}`}); }); });
    record.remote = remote; record.window.contentView.addChildView(remote); remoteBounds(record);
    record.navigationPending = true;
    guardRemote(record, remote.webContents, record.scope.origin);
    await remote.webContents.loadURL(record.scope.origin + route + target.hash);
    if (record.remote !== remote || record.scope.closed || generation !== record.navigationGeneration) throw new DesktopError('desktop.navigation_cancelled', '该导航已由较新的请求替代。');
    await applyRemoteChrome(record, remote.webContents);
    if (record.remote !== remote || generation !== record.navigationGeneration) return;
    remote.setVisible(true);
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
    else if (!preview && /^\/(chat|settings)(\/|$)/.test(url.pathname)) { event.preventDefault(); void navigate(record, url.pathname + url.search + url.hash).catch(error => emit(record, { message: error.message })); }
  };
  contents.on('will-navigate', guard); contents.on('will-redirect', event => { if (event.isMainFrame) guard(event); });
  if (!preview) {
    const navigated = (_event, value, isMainFrame = true) => {
      if (isMainFrame === false || record.remote?.webContents !== contents || record.navigationPending || record.scope?.closed) return;
      const url = parseWebUrl(value);
      if (!url || url.origin !== origin) return;
      let route;
      try { route = safeRoute(url.pathname + url.search + url.hash); }
      catch (error) {
        emit(record, { message: error.message, error: transportError(error) });
        return;
      }
      if (/^\/(chat|settings)(\/|\?|$)/.test(route)) {
        void navigate(record, route + url.hash).catch(error => emit(record, { message: error.message }));
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
const PREVIEW_LOG_LIMIT = 1000;
const previewLogs = new Map();
const watchedPreviewSessions = new WeakSet();
function createPreviewLogs() { return { console: [], network: [], dropped: { console: 0, network: 0 }, started: new Map() }; }
function recordPreviewLog(logs, kind, entry) {
  const rows = logs[kind];
  rows.push({ at: new Date().toISOString(), ...entry });
  if (rows.length > PREVIEW_LOG_LIMIT) { rows.splice(0, rows.length - PREVIEW_LOG_LIMIT); logs.dropped[kind] = (logs.dropped[kind] || 0) + 1; }
}
function watchPreviewNetwork(previewSession) {
  if (watchedPreviewSessions.has(previewSession)) return;
  watchedPreviewSessions.add(previewSession);
  const finish = (details, extra) => {
    const logs = previewLogs.get(details.webContentsId);
    if (!logs) return;
    const started = logs.started.get(details.id); logs.started.delete(details.id);
    recordPreviewLog(logs, 'network', { url: details.url, method: details.method, type: details.resourceType,
      duration_ms: started ? Math.round(details.timestamp - started) : undefined, ...extra });
  };
  previewSession.webRequest.onSendHeaders(details => { previewLogs.get(details.webContentsId)?.started.set(details.id, details.timestamp); });
  previewSession.webRequest.onCompleted(details => finish(details, { status: details.statusCode, from_cache: details.fromCache, failed: details.statusCode >= 400 }));
  previewSession.webRequest.onErrorOccurred(details => finish(details, { error: details.error, failed: true }));
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
  const persistent = input.persistent === true;
  if (previous?.webContents && !previous.webContents.isDestroyed() && previous.scope === record.scope && previous.surfaceId === input.surfaceId && previous.threadId === input.threadId && previous.persistent === persistent) {
    previous.setBounds(bounds); previous.setVisible(true); previous.hidden = false;
    // Layout updates can still carry the old address while an in-page
    // navigation is being reported to React. Keep the live view and history;
    // only a changed renderer request for a different page starts navigation.
    const navigate = previous.requestedUrl !== url.href && previous.url !== url.href
      && previous.webContents.getURL() !== url.href;
    previous.requestedUrl = url.href;
    if (navigate) {
      void previous.webContents.loadURL(url.href).catch(error => {
        if (error.code !== 'ERR_ABORTED' && record.preview === previous && !record.window.isDestroyed()) {
          record.window.webContents.send('desktop:preview-state', { id: previous.id, threadId: previous.threadId,
            url: previous.webContents.getURL() || url.href, title: previous.webContents.getTitle(),
            status: 'error', message: `网页加载失败：${error.code || error.message}` });
        }
      });
    } else if (input.reload) previous.webContents.reload();
    return { id: previous.id };
  }
  disposePreview(record);
  // A persistent profile is shared by every preview of one service origin, so
  // logins survive restarts; the default stays a throwaway in-memory session.
  const partition = persistent
    ? `persist:muteki-preview-${createHash('sha256').update(record.scope.environmentId || record.scope.origin).digest('hex').slice(0, 16)}`
    : `muteki-preview-${record.window.id}-${randomUUID()}`;
  const previewSession = session.fromPartition(partition);
  previewSession.setPermissionRequestHandler((_c, _p, callback) => callback(false));
  previewSession.setPermissionCheckHandler(() => false);
  watchPreviewNetwork(previewSession);
  const preview = new WebContentsView({ webPreferences: webPreferences(partition) });
  // Explicit session beats defaultSession; previews never inherit the trusted
  // renderer's storage, preload, custom transport, or native bridge.
  Object.assign(preview, { id: randomUUID(), surfaceId: input.surfaceId, threadId: input.threadId, scope: record.scope, url: url.href, requestedUrl: url.href, hidden: false, persistent, logs: createPreviewLogs() });
  // view.webContents is already gone when 'destroyed' fires; keep the id.
  const previewContentsId = preview.webContents.id;
  previewLogs.set(previewContentsId, preview.logs);
  preview.webContents.once('destroyed', () => previewLogs.delete(previewContentsId));
  preview.webContents.on('console-message', details => recordPreviewLog(preview.logs, 'console', {
    level: details.level, message: details.message, source: details.sourceId || undefined, line: details.lineNumber || undefined, url: preview.webContents.getURL(),
  }));
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
async function emulateColorScheme(contents, scheme) {
  if (!['light', 'dark', 'system'].includes(scheme)) throw new DesktopError('desktop.preview_scheme_invalid', '外观只能是 light、dark 或 system。');
  const debug = contents.debugger;
  try {
    if (!debug.isAttached()) debug.attach('1.3');
    await debug.sendCommand('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: scheme === 'system' ? '' : scheme }] });
  } catch (error) {
    throw new DesktopError('desktop.preview_scheme_failed', `无法切换页面外观：${error?.message || error}`);
  }
  return { scheme };
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
    const window = new BrowserWindow({ title: environment.name, ...restoreBounds(), minWidth: 360, minHeight: 320, show: false,
      titleBarStyle: 'hidden', ...(process.platform === 'darwin' ? { trafficLightPosition: { x: 16, y: 16 } } : {}), backgroundColor: '#f3f3f3',
      webPreferences: { ...webPreferences(undefined), backgroundThrottling: false, preload: path.join(__dirname, 'preload.cjs') } });
    record = { window, generation: 0, rendererErrors: [], connecting: false, terminals: new Map(), selectedPaths: new Map(), workspaceGrants: new Map(), barriers: new Map(), state: {
      origin: origin || preferences.origin || '', transportOrigin: '', status: 'idle', configuring: true,
      connectionMode: origin ? (managedService?.ready?.origin === origin ? 'local' : 'external') : preferences.mode,
      message: preferences.error?.message || '', error: preferences.error, route: safeRoute(route), platform: process.platform, desktopVersion: app.getVersion(), capabilities: CAPABILITIES,
      environment: { id: environment.id, channel: environment.channel, name: environment.name }, managedService: managedService?.status,
    } };
    Object.assign(record.state, resetNavigation(record, record.state.route));
    if (route !== '/chat') record.pendingRoute = route;
    windows.set(window.id, record);
    window.webContents.on('console-message', details => {
      if (details.level !== 'error') return;
      record.rendererErrors.push({message: details.message, source: details.sourceId, line: details.lineNumber});
      if (environment.channel !== 'stable' || updateJournal) {
        try { fs.appendFileSync(path.join(environment.paths.logs, 'renderer-errors.jsonl'), JSON.stringify(record.rendererErrors.at(-1)) + '\n', {mode: 0o600}); }
        catch (error) { console.error('Cannot persist renderer diagnostics:', error.message); }
      }
    });
    installKeyboard(record, window.webContents);
    window.webContents.on('did-start-navigation', details => { if (details.isMainFrame) nativeSpeech.cancel(record); });
    window.webContents.on('will-navigate', event => {
      event.preventDefault();
      if (local(event.url)) { const target = new URL(event.url); void navigate(record, target.pathname + target.search + target.hash).catch(error => emit(record, { message: error.message })); }
      else { const url = parseWebUrl(event.url); if (url) void openExternal(record, url.href).catch(error => emit(record, { message: error.message })); }
    });
    window.webContents.on('will-frame-navigate', event => {
      if (event.isMainFrame) return;
      try {
        const url = new URL(event.url), document = documents.get(url.host);
        if (url.protocol !== 'muteki-desktop:' || !document || document.record !== record || document.scope !== record.scope || url.pathname !== '/document.html') event.preventDefault();
      } catch { event.preventDefault(); }
    });
    window.webContents.setWindowOpenHandler(({ url }) => { if (local(url)) { const target = new URL(url); void createWindow(record.state.origin, target.pathname + target.search + target.hash).catch(error => emit(record, { message: error.message })); }
      else if (parseWebUrl(url)) void openExternal(record, url).catch(error => emit(record, { message: error.message })); return { action: 'deny' }; });
    window.webContents.on('render-process-gone', () => { disposeConnection(record); emit(record, { status: 'error', configuring: true, message: '聊天界面进程已停止，请重新连接。' }); });
    window.on('resize', () => { remoteBounds(record); if (record.preview) { record.preview.setVisible(false); record.preview.hidden = true; } });
    window.on('show', () => emit(record));
    window.on('hide', () => emit(record));
    window.on('blur', () => emit(record));
    window.on('focus', () => {
      emit(record);
      if (!window.webContents.isDestroyed()) window.webContents.send('desktop:focus');
      void renewManagedSessions().catch(error => emit(record, {message: error.message}));
    });
    window.on('close', event => {
      if (record.allowClose) return;
      event.preventDefault();
      if (record.pendingClose) return;
      const token = randomUUID(); record.pendingClose = token;
      window.webContents.send('desktop:before-close', { token });
      record.closeTimer = setTimeout(() => { record.pendingClose = undefined; quitting = false; quitApproved = false; emit(record, { message: text('草稿保存尚未确认，窗口仍保持打开。请重试关闭。', 'Draft saving was not confirmed. The window remains open; retry closing.') }); }, 5000);
    });
    window.on('closed', () => { clearTimeout(record.closeTimer); for (const barrier of record.barriers.values()) { clearTimeout(barrier.timer); barrier.resolve({ persisted: false, error: '窗口已关闭。' }); } record.barriers.clear(); record.generation++; record.connectionAbort?.abort(); disposeConnection(record); windows.delete(window.id); if (quitting && windows.size === 0) app.quit(); });
    await window.loadURL(SHELL_URL); window.show();
    if (origin) await connect(record, origin);
    else if (preferences.mode === 'local') await connectLocal(record);
    else if (preferences.origin) await connect(record, preferences.origin);
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
    closeScope(scope); handoffNotificationEntries(record); disposeNotifications(record, 'desktop.notification_scope_closed'); disposePreview(record); disposeRemote(record); record.workspaceGrants.clear();
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
      const next = { origin: scope.origin, host, version: attempt, requests: new Set(), closed: false, serviceId: data.service_id, identityId: data.identity_id, auth: authSessions.forService(scope.origin, data.service_id) };
      record.scope = next; scopes.set(host, { record, scope: next });
      record.navigationGeneration = (record.navigationGeneration || 0) + 1;
      emit(record, { transportOrigin: `muteki-desktop://${host}`, connectionVersion: attempt, serviceId: next.serviceId, identityId: next.identityId,
        status: 'connected', configuring: false, candidateOrigin: '', ...resetNavigation(record, '/chat'), draftRecovery: false, message: '服务数据身份已改变，请确认当前服务。原服务草稿按身份隔离保留。' });
    }).catch(error => emit(record, { status: 'error', draftRecovery: true, message: error.message }));
    return;
  }
  const firstIdentity = !scope.serviceId;
  scope.serviceId = data.service_id; scope.identityId = data.identity_id;
  const key = `${scope.environmentId ? 'managed:' + scope.environmentId : scope.origin}|${scope.serviceId}|${scope.identityId}`;
  const route = record.pendingRoute || (!changed && preferences.routes?.[key]) || '/chat'; record.pendingRoute = undefined;
  const restoreRoute = firstIdentity && !record.routeRequested;
  emit(record, { serviceId: scope.serviceId, identityId: scope.identityId, ...(restoreRoute ? resetNavigation(record, safeRoute(route)) : {}) });
  persist(record);
  if (restoreRoute && (new URL(route, SHELL_URL).hash || (!/^\/(chat|settings)(\/|\?|$)/.test(route) && route !== '/'))) {
    void navigate(record, route, 'replace').catch(error => emit(record, { message: error.message, error: transportError(error) }));
  }
}
function notificationScope(record, input) {
  const scope = record.scope;
  if (!scope || scope.closed || record.window.isDestroyed() || !scope.serviceId || !scope.identityId
    || input?.connectionVersion !== scope.version || input?.serviceId !== scope.serviceId || input?.identityId !== scope.identityId) {
    throw new DesktopError('desktop.notification_scope_changed', text('通知授权所属的工作台已改变，请在当前工作台重试。', 'The notification workspace changed. Retry in the current workspace.'));
  }
  return scope;
}
function notificationChoice(record, scope) {
  const key = notificationConsentKey(scope);
  if (!key) return undefined;
  const allowed = preferences.notificationConsents?.[key];
  if (typeof allowed !== 'boolean') return undefined;
  record.notificationPermission = { scope, key, allowed };
  return allowed;
}
async function systemNotificationPermission() {
  if (process.platform !== 'darwin') return { systemPermission: 'unknown' };
  try {
    const native = require(app.isPackaged ? path.join(process.resourcesPath, 'native', 'macos-speech.node') : path.join(__dirname, '..', 'native', 'build', 'macos-speech.node'));
    const settings = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new DesktopError('desktop.notification_status_timeout', 'macOS notification settings did not respond within 5s.')), 5000);
      try { native.notificationSettings(raw => { clearTimeout(timer); try { const result = JSON.parse(raw); if (result.error) { const error = new DesktopError(result.error.code, result.error.message, { cause: Object.assign(new Error(result.error.message), result.error) }); error.nativeDetails = result.error; reject(error); } else resolve(result); } catch (error) { reject(error); } }); }
      catch (error) { clearTimeout(timer); reject(error); }
    });
    const permissions = { notDetermined: 'default', denied: 'denied', authorized: 'granted', provisional: 'provisional', ephemeral: 'ephemeral', unknown: 'unknown' };
    if (!(settings.authorizationStatus in permissions)) throw new DesktopError('desktop.notification_status_invalid', `Invalid macOS authorization status: ${JSON.stringify(settings)}`);
    return { systemPermission: permissions[settings.authorizationStatus], systemNotificationSettings: settings };
  } catch (error) {
    return { systemPermission: 'unknown', systemPermissionError: { ...transportError(error), detail: [error.stack || String(error), error.nativeDetails ? JSON.stringify(error.nativeDetails) : ''].filter(Boolean).join('\n') } };
  }
}
async function notificationStatus(record, input) {
  const scope = notificationScope(record, input);
  // Electron macOS isSupported() initializes its presenter and requests OS
  // authorization. A read-only status query must not call it. macOS supports
  // UserNotifications; actual authorization is read below from the OS.
  const supported = process.platform === 'darwin' ? true : NativeNotification.isSupported();
  const system = await systemNotificationPermission();
  if (notificationScope(record, input) !== scope) throw new DesktopError('desktop.notification_scope_changed', 'Notification workspace changed during system settings query.');
  const choice = notificationChoice(record, scope);
  return { connectionVersion: scope.version, serviceId: scope.serviceId, identityId: scope.identityId,
    permission: !supported ? 'unsupported' : choice === true ? 'granted' : choice === false ? 'denied' : 'default',
    workspacePermission: choice === true ? 'granted' : choice === false ? 'denied' : 'default',
    host: 'desktop-client', ...system,
    ...(record.notificationDiagnostic?.scope === scope ? { delivery: record.notificationDiagnostic.value } : {}),
    code: !supported ? 'desktop.notifications_unsupported' : choice === true ? 'desktop.notifications_workspace_allowed' : choice === false ? 'desktop.notifications_workspace_denied' : 'desktop.notifications_not_requested' };
}
async function requestNotifications(record, input) {
  const scope = notificationScope(record, input), status = await notificationStatus(record, input);
  if (status.permission === 'unsupported') return status;
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
        buttons: [text('取消', 'Cancel'), text('不允许', 'Do not allow'), text('允许', 'Allow')], defaultId: 0, cancelId: 0 });
      if (notificationScope(record, input) !== scope) throw new DesktopError('desktop.notification_scope_changed', 'Notification workspace changed.');
      if (response === 0) return notificationStatus(record, input);
      if (response !== 1 && response !== 2) throw new DesktopError('desktop.notification_reply_invalid', 'Notification permission dialog returned an invalid choice.');
      const key = notificationConsentKey(scope), allowed = response === 2;
      const nextPreferences = { ...preferences, notificationConsents: { ...preferences.notificationConsents, [key]: allowed } };
      try {
        writePreferences(preferencesFile, nextPreferences.origin || scope.origin, nextPreferences);
      } catch (cause) {
        throw new DesktopError('desktop.notification_preferences_failed',
          `${text('通知选择未能保存，授权保持原状态。', 'The notification choice could not be saved; permission is unchanged.')} ${cause.message || String(cause)}`,
          { retryable: true, cause });
      }
      preferences = nextPreferences;
      record.notificationPermission = { scope, key, allowed };
      if (!allowed) for (const owner of windows.values()) {
        if (owner.scope && !owner.scope.closed && notificationConsentKey(owner.scope) === key) {
          disposeNotifications(owner, 'desktop.notifications_workspace_denied');
        }
      }
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
    && notificationChoice(record, record.scope) === true);
}
function notificationEvent(entry, status, code, message) {
  const { record, scope } = entry;
  entry.history.push({ status, at: new Date().toISOString(), ...(code ? { code } : {}), ...(message ? { message } : {}) });
  const value = { connectionVersion: scope.version, serviceId: scope.serviceId, identityId: scope.identityId,
    id: entry.id, threadId: entry.threadId, eventId: entry.eventId, dedupeKey: entry.dedupeKey,
    seq: record.notificationSequence = (record.notificationSequence || 0) + 1, status, shown: entry.shown, ...(entry.system || {}), soundRequested: entry.soundRequested === true, shownReceiptMeaning: process.platform === 'darwin' ? 'system-accepted-scheduling' : 'native-show-event',
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
function playNotificationSound(record, input) {
  const scope = notificationScope(record, input);
  if (!isNotificationOwner(record)) throw new DesktopError('desktop.notification_owner_changed', 'This window no longer owns workspace notifications.');
  if (!notificationsAllowed(record)) throw new DesktopError('desktop.notifications_workspace_denied', 'This workspace has not allowed native notification sounds.');
  if (!input || ['threadId', 'eventId', 'dedupeKey'].some(key => typeof input[key] !== 'string' || !input[key])) throw new DesktopError('desktop.notification_sound_invalid', 'The sound request is missing its thread or event identity.');
  const workspace = notificationConsentKey(scope), key = JSON.stringify([workspace, input.dedupeKey]);
  const visible = [...windows.values()].some(owner => owner.scope && !owner.scope.closed && !owner.window.isDestroyed()
    && notificationConsentKey(owner.scope) === workspace && !owner.state.configuring && !owner.remote && owner.window.isVisible() && !owner.window.isMinimized() && notificationWindowFocused(owner) && activeThread(owner, input.threadId));
  const status = visible ? 'suppressed' : requestedNotificationSounds.has(key) ? 'already-requested' : 'sound-requested';
  if (status === 'sound-requested') {
    try { shell.beep(); }
    catch (cause) { throw new DesktopError('desktop.notification_sound_failed', cause.stack || cause.message || String(cause), { cause }); }
    requestedNotificationSounds.add(key);
  }
  const receipt = { connectionVersion: scope.version, serviceId: scope.serviceId, identityId: scope.identityId,
    threadId: input.threadId, eventId: input.eventId, dedupeKey: input.dedupeKey, status, host: 'desktop-client' };
  console.info('muteki.notification.sound', JSON.stringify(receipt));
  return receipt;
}
async function sendNotification(record, input) {
  const scope = notificationScope(record, input);
  if (!isNotificationOwner(record)) throw new DesktopError('desktop.notification_owner_changed', 'This window no longer owns workspace notifications.');
  if (!notificationsAllowed(record)) throw new DesktopError('desktop.notifications_workspace_denied', text('此工作台尚未获得通知授权。', 'This workspace has not been allowed to send notifications.'));
  if (!input || ['threadId', 'eventId', 'dedupeKey', 'title', 'body'].some(key => typeof input[key] !== 'string') || !input.threadId || !input.eventId || !input.dedupeKey) {
    throw new DesktopError('desktop.notification_input_invalid', 'The notification is missing its thread or event identity.');
  }
  if (input.wantSound !== undefined && typeof input.wantSound !== 'boolean') throw new DesktopError('desktop.notification_sound_invalid', 'wantSound must be a boolean.');
  const system = await systemNotificationPermission();
  // OS querying is asynchronous: revalidate every authority before acting.
  if (notificationScope(record, input) !== scope) throw new DesktopError('desktop.notification_scope_changed', 'Notification workspace changed during system settings query.');
  if (!isNotificationOwner(record)) throw new DesktopError('desktop.notification_owner_changed', 'This window no longer owns workspace notifications.');
  if (!notificationsAllowed(record)) throw new DesktopError('desktop.notifications_workspace_denied', 'This workspace has not allowed notifications.');
  const workspace = notificationConsentKey(scope);
  if ([...windows.values()].some(owner => owner.scope && !owner.scope.closed && notificationConsentKey(owner.scope) === workspace
    && !owner.state.configuring && !owner.remote && owner.window.isVisible() && !owner.window.isMinimized() && notificationWindowFocused(owner) && activeThread(owner, input.threadId))) {
    const suppressed = { id: randomUUID(), record, scope, threadId: input.threadId, eventId: input.eventId, dedupeKey: input.dedupeKey, history: [], shown: false, active: false };
    notificationEvent(suppressed, 'closed', 'desktop.notification_thread_visible', 'This thread is already visible in a workspace window.');
    return suppressed.last;
  }
  if (system.systemPermission === 'denied') {
    const suppressed = { id: randomUUID(), record, scope, threadId: input.threadId, eventId: input.eventId,
      dedupeKey: input.dedupeKey, history: [], shown: false, active: false, system };
    notificationEvent(suppressed, 'closed', 'desktop.notifications_system_denied', 'The operating system has denied notification authorization; no native notification was constructed.');
    return suppressed.last;
  }
  const route = safeRoute(`/chat/${encodeURIComponent(input.threadId)}`);
  if (!NativeNotification.isSupported()) throw new DesktopError('desktop.notifications_unsupported', 'Native notifications are unsupported on this system.');
  const deliveredKey = JSON.stringify([workspace, input.dedupeKey]);
  const delivered = deliveredNotificationEvents.get(deliveredKey);
  if (delivered) return { ...delivered, connectionVersion: scope.version, serviceId: scope.serviceId, identityId: scope.identityId,
    seq: record.notificationSequence = (record.notificationSequence || 0) + 1, requestedEventId: input.eventId, code: 'desktop.notification_already_delivered' };
  record.notifications ||= new Map();
  const previous = record.notifications.get(input.threadId);
  if (previous?.scope === scope && previous.dedupeKey === input.dedupeKey) return { ...previous.last, requestedEventId: input.eventId };
  if (previous) {
    notificationEvent(previous, 'closed', 'desktop.notification_replaced', 'Replaced by a new notification from this thread.');
    releaseNotification(previous, true);
  }
  const entry = { id: randomUUID(), record, scope, threadId: input.threadId, eventId: input.eventId,
    dedupeKey: input.dedupeKey, system, soundRequested: input.wantSound === true, history: [], shown: false, active: true };
  record.notifications.set(entry.threadId, entry);
  notificationEvent(entry, 'submitted');
  const sameScope = () => entry.record.scope === entry.scope && !entry.scope.closed && !entry.record.window.isDestroyed()
    && input.serviceId === entry.scope.serviceId && input.identityId === entry.scope.identityId;
  const current = () => entry.active && sameScope();
  try {
    entry.note = new NativeNotification({ title: input.title, body: input.body, silent: input.wantSound !== true });
    entry.note.on('show', () => {
      if (!current()) return;
      clearTimeout(entry.timer); entry.shown = true;
      notificationEvent(entry, 'shown');
      deliveredNotificationEvents.set(deliveredKey, entry.last);
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
      if (entry.record.window.isMinimized()) entry.record.window.restore();
      entry.record.window.show(); entry.record.window.focus();
      notificationEvent(entry, 'clicked');
      deliveredNotificationEvents.set(deliveredKey, entry.last);
      void navigate(entry.record, route).then(() => {
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
// GUI-launched apps on macOS do not inherit the login shell PATH, so the
// usual CLI install locations are probed explicitly.
const EDITOR_CLIS = [
  ['code', 'VS Code', ['/usr/local/bin/code', '/opt/homebrew/bin/code', '/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code']],
  ['cursor', 'Cursor', ['/usr/local/bin/cursor', '/opt/homebrew/bin/cursor', '/Applications/Cursor.app/Contents/Resources/app/bin/cursor']],
  ['windsurf', 'Windsurf', ['/usr/local/bin/windsurf', '/opt/homebrew/bin/windsurf', '/Applications/Windsurf.app/Contents/Resources/app/bin/windsurf']],
  ['zed', 'Zed', ['/usr/local/bin/zed', '/opt/homebrew/bin/zed', '/Applications/Zed.app/Contents/MacOS/cli']],
];
const EDITOR_CHOICES = new Set(['auto', 'system', ...EDITOR_CLIS.map(([name]) => name)]);
function findEditorCli(preferred = 'auto') {
  // Windows editor shims are .cmd scripts that cannot be spawned without a shell.
  if (process.platform === 'win32') return null;
  const dirs = String(process.env.PATH || '').split(path.delimiter).filter(Boolean);
  for (const [name, label, extra] of EDITOR_CLIS) {
    if (preferred !== 'auto' && preferred !== name) continue;
    for (const candidate of [...dirs.map(dir => path.join(dir, name)), ...extra]) {
      try { fs.accessSync(candidate, fs.constants.X_OK); return { name, label, file: candidate }; } catch { /* next candidate */ }
    }
  }
  return null;
}
async function openInEditor(target, line, preferred = 'auto') {
  const cli = preferred === 'system' ? null : findEditorCli(preferred);
  if (!cli && preferred !== 'auto' && preferred !== 'system') {
    const label = EDITOR_CLIS.find(([name]) => name === preferred)?.[1] || preferred;
    throw new DesktopError('desktop.editor_not_found', text(`未找到 ${label} 命令行，请安装后重试，或在设置中改用其他编辑器。`, `${label} command line was not found. Install it or choose another editor in Settings.`));
  }
  if (cli) {
    const env = { ...process.env }; delete env.ELECTRON_RUN_AS_NODE;
    // Zed takes path:line directly; the VS Code family needs --goto.
    const args = !line ? [target] : cli.name === 'zed' ? [`${target}:${line}`] : ['--goto', `${target}:${line}`];
    await new Promise((resolve, reject) => {
      const child = spawn(cli.file, args, { detached: true, stdio: 'ignore', env });
      child.once('error', error => reject(new DesktopError('desktop.editor_launch_failed', `${cli.label} 启动失败：${error.message}`)));
      child.once('spawn', () => { child.unref(); resolve(); });
    });
    return { opener: cli.name, label: cli.label, path: target };
  }
  const error = await shell.openPath(target);
  if (error) throw new DesktopError('desktop.workspace_file_open_failed', error);
  return { opener: 'system', label: text('系统默认应用', 'system default application'), path: target };
}
function installIPC() {
  const handle = (name, fn) => ipcMain.handle(name, async (event, ...args) => {
    const record = windows.get(BrowserWindow.fromWebContents(event.sender)?.id);
    if (!record || event.sender !== record.window.webContents || event.senderFrame !== event.sender.mainFrame || !local(event.senderFrame.url)) return { ok: false, error: transportError(new DesktopError('desktop.sender_untrusted', '此页面不能使用桌面接口。')) };
    try { return { ok: true, value: await fn(record, ...args) }; }
    catch (error) { return { ok: false, error: transportError(error) }; }
  });
  handle('desktop:state', record => ({ ...record.state })); handle('desktop:connect', connect);
  handle('desktop:update-status', () => updatesController().getStatus());
  handle('desktop:update-check', () => updatesController().check());
  handle('desktop:update-install', async () => {
    const controller = updatesController();
    if (preparingUpdate) throw new DesktopError('update.busy', '更新流程正在进行中。');
    preparingUpdate = true;
    try {
      const ready = await controller.download();
      if (ready.state !== 'ready') return ready;
      const { response } = await dialog.showMessageBox({
        type: 'question',
        message: text(`版本 ${ready.latestVersion} 已验证，可以安装`, `Version ${ready.latestVersion} is verified and ready to install`),
        detail: text('安装会等待当前任务结束、保存草稿并重启。日常数据将先备份。', 'Installation saves drafts, waits for active work to finish, backs up daily data, and restarts.'),
        buttons: [text('稍后', 'Later'), text('应用更新', 'Install update')],
        defaultId: 0,
        cancelId: 0,
      });
      if (response !== 1) return controller.publish({ state: 'ready', message: '更新包已验证。' });
      await applyPreparedUpdate();
      return controller.publish({ state: 'installing', message: '正在退出并安装更新。' });
    } finally {
      preparingUpdate = false;
    }
  });
  handle('desktop:connect-local', connectLocal);
  handle('desktop:configure', record => { disposePreview(record); record.remote?.setVisible(false); emit(record, { configuring: true }); });
  handle('desktop:resume', record => { if (record.scope && (!record.scope.closed || record.state.draftRecovery)) { record.remote?.setVisible(true); emit(record, { configuring: false }); } });
  handle('desktop:navigate', navigate);
  handle('desktop:anchor-consumed', (record, id) => { if (record.state.anchorTarget?.id === id) emit(record, {anchorTarget: undefined}); });
  handle('desktop:action', (record, name) => { if (!['back', 'forward', 'reload', 'sidebar', 'search', 'theme', 'new-chat', 'settings', 'shortcuts'].includes(name)) throw new DesktopError('desktop.command_invalid', '此命令不可用。'); return executeCommand(name, record); });
  handle('desktop:appearance', (record, input) => { record.appearance = parseAppearance(input); return applyRemoteAppearance(record); });
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
    else if (input.action === 'reload-hard') contents.reloadIgnoringCache();
    else if (input.action === 'devtools') { const wasOpen = contents.isDevToolsOpened(); if (wasOpen) contents.closeDevTools(); else contents.openDevTools({ mode: 'detach' }); return { open: !wasOpen }; }
    else if (input.action === 'zoom') {
      const factor = Number(input.factor);
      if (!Number.isFinite(factor) || factor < 0.25 || factor > 3) throw new DesktopError('desktop.preview_zoom_invalid', '页面缩放须在 25% 到 300% 之间。');
      contents.setZoomFactor(factor); return { factor };
    }
    else if (input.action === 'color-scheme') return emulateColorScheme(contents, input.scheme);
    else if (input.action === 'screenshot') return contents.capturePage().then(image => {
      if (image.isEmpty()) throw new DesktopError('desktop.preview_capture_empty', '截图为空：预览可能尚未绘制。', { retryable: true });
      const size = image.getSize();
      return { mime: 'image/png', data: image.toPNG().toString('base64'), width: size.width, height: size.height, url: contents.getURL(), title: contents.getTitle() };
    });
    else if (input.action === 'clear-data') {
      if (!preview.persistent) throw new DesktopError('desktop.preview_not_persistent', '当前预览未保留登录状态，没有需要清除的数据。');
      const target = contents.session;
      return Promise.all([target.clearStorageData(), target.clearCache(), target.clearAuthCache()]).then(() => { contents.reload(); return { cleared: true }; });
    }
    else if (input.action === 'pick') return pickElement(preview);
    else if (input.action === 'pick-cancel') return cancelPick(preview);
    else throw new DesktopError('desktop.preview_action_unavailable', '此预览操作不可用。');
  });
  handle('desktop:browser-control', createPreviewControl({ activeThread }));
  handle('desktop:close-ack', async (record, input) => {
    const barrier = record.barriers.get(input?.token);
    if (barrier) { clearTimeout(barrier.timer); record.barriers.delete(input.token); barrier.resolve({ persisted: input.persisted === true, error: input.error }); return; }
    if (!input || input.token !== record.pendingClose) throw new DesktopError('desktop.close_ack_stale', '关闭请求已改变。');
    clearTimeout(record.closeTimer); record.pendingClose = undefined;
    if (!input.persisted) { const { response } = await dialog.showMessageBox(record.window, { type: 'warning', message: text('草稿保存失败。仍然关闭窗口？', 'Draft saving failed. Close the window anyway?'), detail: String(input.error || ''), buttons: [text('保持打开', 'Keep open'), text('关闭', 'Close')], defaultId: 0, cancelId: 0 }); if (response !== 1) { quitting = false; quitApproved = false; return; } }
    persist(record);
    if (process.platform === 'darwin' && !quitting && (isNotificationOwner(record) || windows.size === 1)) { record.window.hide(); return; }
    record.allowClose = true; record.window.close();
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
  handle('desktop:workspace-editor', async (record, input) => {
    const grant = record.workspaceGrants.get(input?.grantId), scope = record.scope;
    if (!grant || grant.scope !== scope || scope.closed || !activeThread(record, grant.threadId) || ['threadId', 'workspaceId', 'serviceId', 'identityId'].some(key => input[key] !== grant[key])) throw new DesktopError('desktop.workspace_scope_changed', '本机目录映射或会话已改变，请重新选择。');
    const relative = input.relativePath ?? '';
    if (typeof relative !== 'string' || path.isAbsolute(relative) || (input.line !== undefined && !(Number.isInteger(input.line) && input.line > 0))) throw new DesktopError('desktop.workspace_file_invalid', '请选择工作区内的相对文件路径。');
    const editor = input.editor ?? 'auto';
    if (typeof editor !== 'string' || !EDITOR_CHOICES.has(editor)) throw new DesktopError('desktop.editor_invalid', '不支持的编辑器选项。');
    const root = await fs.promises.realpath(grant.clientRoot), target = relative ? await fs.promises.realpath(path.resolve(root, relative)) : root;
    if (root !== grant.clientRoot || (target !== root && !target.startsWith(root + path.sep))) throw new DesktopError('desktop.workspace_file_outside', '文件不属于已选择的本机工作区。');
    if (scope !== record.scope || scope.closed || !activeThread(record, grant.threadId)) throw new DesktopError('desktop.connection_changed', '服务连接或会话已改变。');
    return openInEditor(target, relative && input.line ? input.line : undefined, editor);
  });
  handle('desktop:menu-accelerators', (_record, input) => {
    const next = { ...DEFAULT_MENU_ACCELERATORS };
    for (const key of Object.keys(DEFAULT_MENU_ACCELERATORS)) {
      const value = input?.[key];
      if (value === undefined || value === null) continue;
      // An empty accelerator leaves the shortcut to the renderer (bindings Electron cannot express, such as `?`).
      if (typeof value !== 'string' || (value !== '' && !ACCELERATOR_PATTERN.test(value))) throw new DesktopError('desktop.accelerator_invalid', `无效的菜单快捷键：${String(value).slice(0, 40)}`);
      next[key] = value;
    }
    menuAccelerators = next;
    installMenu();
    return menuAccelerators;
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
  handle('desktop:notification-sound', playNotificationSound);
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
const DEFAULT_MENU_ACCELERATORS = { 'new-chat': 'CmdOrCtrl+Shift+N', search: 'CmdOrCtrl+K', sidebar: 'CmdOrCtrl+B' };
let menuAccelerators = { ...DEFAULT_MENU_ACCELERATORS };
const ACCELERATOR_PATTERN = /^(?:(?:CmdOrCtrl|Ctrl|Alt|Shift)\+){0,4}(?:[A-Z0-9]|F[1-9]|F1[0-2]|Up|Down|Left|Right|Enter|Escape|Space|Tab|Backspace|Delete|[\[\],./\\;'`=-])$/;
function installMenu() {
  const role = (name, zh, en) => ({ role: name, label: text(zh, en) });
  const item = (zh, en, name, accelerator) => ({ label: text(zh, en), accelerator: accelerator || undefined, click: () => void executeCommand(name).catch(error => { const record = focused(); if (record) emit(record, { message: error.message }); }) });
  Menu.setApplicationMenu(Menu.buildFromTemplate([
    ...(process.platform === 'darwin' ? [{ label: environment.name, submenu: [role('about', '关于 Muteki', 'About Muteki'), { type: 'separator' }, item('设置…', 'Settings…', 'settings', 'CmdOrCtrl+,'), { label: text('服务连接…', 'Service connection…'), click: () => { const record = focused(); if (record) emit(record, { configuring: true }); } }, {label: text('从本机候选版本更新…', 'Update from a local candidate…'), enabled: app.isPackaged && environment.channel === 'stable', click: () => void installCandidate().catch(error => dialog.showErrorBox(environment.name, error.message))}, { type: 'separator' }, role('hide', '隐藏 Muteki', 'Hide Muteki'), role('hideOthers', '隐藏其他应用', 'Hide others'), role('unhide', '显示全部', 'Show all'), { type: 'separator' }, role('quit', '退出 Muteki', 'Quit Muteki')] }] : []),
    { label: text('对话', 'Chat'), submenu: [item('新建对话', 'New chat', 'new-chat', menuAccelerators['new-chat']), item('搜索对话', 'Search conversations', 'search', menuAccelerators.search), { label: text('新建窗口', 'New window'), click: () => void createWindow(focused()?.state.origin).catch(error => dialog.showErrorBox('Muteki', error.message)) }, { type: 'separator' }, role('close', '关闭窗口', 'Close window')] },
    { label: text('编辑', 'Edit'), submenu: [role('undo', '撤销', 'Undo'), role('redo', '重做', 'Redo'), { type: 'separator' }, role('cut', '剪切', 'Cut'), role('copy', '拷贝', 'Copy'), role('paste', '粘贴', 'Paste'), role('selectAll', '全选', 'Select all')] },
    { label: text('视图', 'View'), submenu: [item('后退', 'Back', 'back', 'Alt+Left'), item('前进', 'Forward', 'forward', 'Alt+Right'), item('刷新', 'Reload', 'reload', 'CmdOrCtrl+R'), item('切换会话侧栏', 'Toggle chat sidebar', 'sidebar', menuAccelerators.sidebar), item('快捷键', 'Keyboard shortcuts', 'shortcuts'), { type: 'separator' }, role('resetZoom', '实际大小', 'Actual size'), role('zoomIn', '放大', 'Zoom in'), role('zoomOut', '缩小', 'Zoom out'), role('togglefullscreen', '切换全屏', 'Toggle fullscreen')] }, role('windowMenu', '窗口', 'Window'),
  ]));
}
let preparingUpdate = false;
async function installCandidate() {
  if (preparingUpdate) return;
  preparingUpdate = true;
  try {
    const picked = await dialog.showOpenDialog({title: text('选择已构建的 Muteki 候选应用', 'Select a built Muteki candidate'), properties: ['openFile'], filters: [{name: 'Muteki', extensions: ['app']}]});
    if (picked.canceled || picked.filePaths.length !== 1) return;
    desktopUpdates = updatesController();
    const prepared = await desktopUpdates.prepare(picked.filePaths[0]);
    app.setAsDefaultProtocolClient('muteki');
    const {response} = await dialog.showMessageBox({type: 'question', message: text(`候选版本 ${prepared.version} 已通过启动验收`, `Candidate ${prepared.version} passed startup acceptance`), detail: text('应用更新会等待当前任务结束、保存草稿并重启。日常数据将先备份，开发数据不会导入。', 'The update saves drafts and restarts after current tasks finish. Daily data is backed up; development data is not imported.'), buttons: [text('稍后', 'Later'), text('应用更新', 'Apply update')], defaultId: 0, cancelId: 0});
    if (response !== 1) {
      desktopUpdates.discardPreparedLocal();
      return;
    }
    await applyPreparedUpdate();
  } finally { preparingUpdate = false; }
}
async function applyPreparedUpdate() {
  const records = [...windows.values()];
  try {
    for (const record of records) {
      record.window.hide();
      const saved = await persistenceBarrier(record, 'update');
      if (!saved.persisted) throw new Error(saved.error || '草稿未保存，更新已暂停。');
    }
    if (managedService?.ready) await managedService.drain();
    for (const record of records) record.allowClose = true;
    applyUpdate = true;
    quitApproved = true;
    app.quit();
  } catch (error) {
    for (const record of records) if (!record.window.isDestroyed()) record.window.show();
    throw error;
  }
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
  app.on('did-become-active', refreshNotificationOwners);
  app.on('did-resign-active', refreshNotificationOwners);
  let servicesStopped = false, quitChecking = false, finishingQuit = false, stopDevControl;
  app.on('before-quit', event => {
    if (managedService?.ready && !quitApproved) {
      event.preventDefault();
      if (quitChecking) return;
      quitChecking = true;
      void managedService.activity().then(async activity => {
        if (activity.active > 0) {
          const { response } = await dialog.showMessageBox({ type: 'question', message: text('仍有任务正在运行', 'Tasks are still running'), detail: text(`退出将停止 ${activity.active} 个活动任务。关闭窗口可让任务继续在后台运行。`, `Quitting will stop ${activity.active} active tasks. Close the window to keep them running in the background.`), buttons: [text('继续运行', 'Keep running'), text('停止任务并退出', 'Stop tasks and quit')], defaultId: 0, cancelId: 0 });
          if (response !== 1) return;
        }
        quitApproved = true; app.quit();
      }).catch(error => dialog.showErrorBox(environment.name, error.message)).finally(() => { quitChecking = false; });
    } else if (windows.size) { event.preventDefault(); quitting = true; for (const record of windows.values()) record.window.close(); }
    else if (managedService && !servicesStopped) {
      event.preventDefault();
      if (finishingQuit) return;
      finishingQuit = true;
      void managedService.stop().then(async result => {
        if (applyUpdate) {
          if (!result.graceful) throw new Error('工作台未能正常停止，本次更新已取消。请重新启动后检查运行状态。');
          await desktopUpdates.launch(); applyUpdate = false;
        }
        servicesStopped = true; finishingQuit = false; app.quit();
      }).catch(error => { finishingQuit = false; applyUpdate = false; servicesStopped = true; dialog.showErrorBox(environment.name, error.message); app.quit(); });
    }
    else if (applyUpdate) { event.preventDefault(); if (finishingQuit) return; finishingQuit = true; void desktopUpdates.launch().then(() => { applyUpdate = false; finishingQuit = false; app.quit(); }).catch(error => { finishingQuit = false; applyUpdate = false; dialog.showErrorBox(environment.name, error.message); }); }
    else stopDevControl?.();
  });
  app.on('activate', () => {
    const existing = [...windows.values()].find(record => !record.window.isDestroyed());
    if (existing) { existing.window.show(); existing.window.focus(); }
    else void createWindow().catch(error => dialog.showErrorBox('Muteki 无法打开窗口', error.message));
  });
  app.whenReady().then(async () => {
    if (interruptedUpdate && !repairingUpdate) {
      if (helperRunning(interruptedUpdate.journal)) {
        dialog.showErrorBox(environment.name, '版本更新仍在进行，请等待更新完成后再打开工作台。');
      } else {
        const {DesktopUpdates} = require('./updates.cjs');
        desktopUpdates = new DesktopUpdates(app, environment);
        desktopUpdates.prepared = {file: interruptedUpdate.file, transaction: path.dirname(interruptedUpdate.file)};
        await desktopUpdates.launch(true);
      }
      app.quit(); return;
    }
    authSessions = new ServiceAuthSessions(path.join(app.getPath('userData'), 'auth-sessions.json'), safeStorage, (owner, source) => {
      for (const record of windows.values()) {
        if (record.scope?.auth === owner && record.scope !== source) emit(record, { authSessionVersion: owner.revision });
      }
    });
    const root = path.resolve(__dirname, '../renderer-dist');
    const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png', '.svg': 'image/svg+xml', '.woff2': 'font/woff2', '.ico': 'image/x-icon' };
    protocol.handle('muteki-desktop', async request => {
      const url = new URL(request.url);
      const document = documents.get(url.host);
      if (document && document.scope === document.record.scope && !document.scope.closed && request.method === 'GET' && url.pathname === '/document.html') {
        return new Response(document.html, { headers: { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff',
          'Content-Security-Policy': "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline' https://cdnjs.cloudflare.com https://esm.sh https://cdn.jsdelivr.net https://unpkg.com; style-src 'unsafe-inline' https://cdnjs.cloudflare.com https://esm.sh https://cdn.jsdelivr.net https://unpkg.com https://fonts.googleapis.com https://fonts.bunny.net; font-src https://fonts.gstatic.com https://fonts.bunny.net; img-src data: blob: https://cdnjs.cloudflare.com https://esm.sh https://cdn.jsdelivr.net https://unpkg.com; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors muteki-desktop://app" } });
      }
      if (url.host !== 'app') {
        const entry = scopes.get(url.host);
        try { if (entry?.scope.environmentId) await renewManagedSessions(); }
        catch (error) { return errorResponse(transportError(error)); }
        return forwardService(request, entry?.scope, (scope, data) => identity(entry.record, scope, data));
      }
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
    if (app.isPackaged && environment.channel === 'stable' && process.env.MUTEKI_DESKTOP_REGISTER_PROTOCOL !== '0') app.setAsDefaultProtocolClient('muteki');
    installIPC(); installMenu();
    const renewal = setInterval(() => { void renewManagedSessions().catch(error => {
      for (const record of windows.values()) if (record.scope?.environmentId === environment.id) emit(record, {message: error.message});
    }); }, 60000);
    renewal.unref(); app.once('will-quit', () => clearInterval(renewal));
    if (process.argv.includes('--muteki-control')) {
      const { startDevControl } = require('./dev-control.cjs');
      stopDevControl = await startDevControl(environment, () => [...windows.values()].flatMap(record => {
        const managed = !record.scope || record.scope.environmentId === environment.id;
        const rawKeyWindowFocused = record.window.isFocused();
        const windowState = { visible: record.window.isVisible(), focused: notificationWindowFocused(record),
          rawKeyWindowFocused, macAppActive: process.platform === 'darwin' ? app.isActive() : null,
          minimized: record.window.isMinimized() };
        return [{contents: record.window.webContents, windowId: record.window.id, kind: 'shell', managed, windowState, revision: `${record.state.route}:${record.generation}`, errors: record.rendererErrors},
          ...(record.remote ? [{contents: record.remote.webContents, windowId: record.window.id, kind: 'workspace', managed}] : []),
          ...(record.preview ? [{contents: record.preview.webContents, windowId: record.window.id, kind: 'browser', managed}] : [])].filter(item => !item.contents.isDestroyed());
      }), () => managedService?.status || {state: 'stopped'}, async () => {
        const activity = await managedService?.activity();
        if (activity?.active) throw Object.assign(new Error('Dev still has active tasks; stop them before closing this instance'), {code: 'dev.tasks_active'});
        setTimeout(() => app.quit(), 100);
      });
    }
    if (updateJournal) {
      const runtime = await managedRuntime().start();
      const auth = await fetch(`${runtime.origin}/api/auth/me`, {headers: {Authorization: `Bearer ${runtime.token}`}, signal: AbortSignal.timeout(10000)});
      if (!auth.ok) throw new Error('Updated workspace failed its private session check');
      patchUpdate({phase: 'validated'});
      if (repairingUpdate) patchUpdate({phase: 'writes-open'});
      const deadline = Date.now() + 30000;
      while (JSON.parse(fs.readFileSync(updateJournal, 'utf8')).phase !== 'writes-open') {
        if (Date.now() > deadline) throw new Error('Update coordinator did not admit writes');
        await new Promise(resolve => setTimeout(resolve, 100));
      }
      await managedRuntime().refreshSession('activate'); environment.maintenance = false;
      await createWindow(preferences.mode === 'external' ? preferences.origin : runtime.origin);
      patchUpdate({phase: 'complete', completedAt: new Date().toISOString()});
    } else if (selfCheck) {
      const record = await createWindow();
      await connectLocal(record);
      const deadline = Date.now() + 20000;
      while (!record.scope?.identityId) {
        if (Date.now() > deadline || record.state.status === 'error') throw new Error('Candidate renderer did not complete its authenticated startup');
        await new Promise(resolve => setTimeout(resolve, 100));
      }
      while (!await record.window.webContents.executeJavaScript('Boolean(document.querySelector(\'[role="textbox"][contenteditable="true"]\'))')) {
        if (Date.now() > deadline) throw new Error('Candidate conversation interface did not render');
        await new Promise(resolve => setTimeout(resolve, 100));
      }
      if (record.rendererErrors.length) throw new Error(`Candidate renderer reported errors; inspect ${path.join(environment.paths.logs, 'renderer-errors.jsonl')}`);
      atomicJson(path.join(environment.root, 'candidate-result.json'), {status: 'passed', environmentId: environment.id, generation: environment.generation, version: app.getVersion(), serviceId: record.scope.serviceId});
      app.quit();
    } else if (pendingLinks.length) { for (const link of pendingLinks) await deepLink(link); }
    else await createWindow(process.env.MUTEKI_DESKTOP_URL || '');
  }).catch(error => {
    if (selfCheck) atomicJson(path.join(environment.root, 'candidate-result.json'), {status: 'failed', error: error.message});
    else if (updateJournal) {
      patchUpdate({phase: repairingUpdate ? 'repair-required' : 'failed', error: error.message});
      if (repairingUpdate) dialog.showErrorBox(environment.name, `当前版本恢复启动失败，数据未回退。\n${error.message}\n${updateJournal}`);
    }
    else dialog.showErrorBox('Muteki 无法启动', error.message || '请构建桌面界面后重试。');
    for (const record of windows.values()) { record.allowClose = true; record.window.destroy(); }
    if (selfCheck || updateJournal) app.quit();
  });
}
