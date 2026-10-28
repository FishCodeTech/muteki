const { test } = require('node:test');
const assert = require('node:assert/strict');
const { normalizeDesktopRoute } = require('../src/policy.cjs');
const { forwardService, closeScope } = require('../src/transport.cjs');

test('chat, run and settings routes preserve their own path and query', () => {
  for (const route of ['/chat/example?panel=files', '/run/run-123?panel=evidence', '/run/run-123/collaboration', '/settings/appearance', '/settings/agent-extensions', '/settings/notifications']) assert.equal(normalizeDesktopRoute(route), route);
  assert.equal(normalizeDesktopRoute('muteki-desktop://app/chat/example'), '/chat/example');
});

test('navigation excludes service API documents and routes outside the app', () => {
  for (const route of ['/api/threads', 'https://example.com/chat', '/assets/bundle.js']) assert.throws(() => normalizeDesktopRoute(route), { code: 'desktop.route_invalid' });
});

test('native service transport preserves complete streamed response bodies', async () => {
  const originalFetch = global.fetch;
  const data = 'synthetic transcript\n'.repeat(16000);
  const scope = { origin: 'http://127.0.0.1:18193', host: 'service-synthetic', requests: new Set(), closed: false };
  global.fetch = async () => new Response(new ReadableStream({ start(controller) { controller.enqueue(new TextEncoder().encode(data)); controller.close(); } }), { headers: { 'content-type': 'application/json' } });
  try {
    const response = await forwardService(new Request('muteki-desktop://service-synthetic/api/threads'), scope, () => {});
    assert.equal(await response.text(), data);
    assert.equal(scope.requests.size, 0);
  } finally { closeScope(scope); global.fetch = originalFetch; }
});

test('native login owns bearer sessions and strips them from renderer responses', async () => {
  const originalFetch = global.fetch;
  let stored = '', forwarded = '';
  const auth = {
    serviceId: 'qa-service', revision: 0, persistenceWarning: '',
    snapshot() { return { token: stored, revision: this.revision }; },
    accept(data) { stored = data.token; this.revision += 1; },
    clear(snapshot) { if (snapshot.revision === this.revision) stored = ''; },
  };
  const scope = { origin: 'http://127.0.0.1:18193', host: 'service-private', requests: new Set(), closed: false, auth };
  global.fetch = async (_target, init) => {
    forwarded = init.headers.get('Authorization');
    return Response.json({ ok: true, auth_required: true, service_id: 'qa-service', identity_id: 'operator',
      session_protocol: 2, session_owner: 'desktop', token: 'synthetic-private-session', expires_at: 9999999999 });
  };
  try {
    const response = await forwardService(new Request('muteki-desktop://service-private/api/auth/login', {
      method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: 'Bearer renderer-token' },
      body: JSON.stringify({ password: 'synthetic-password', client: 'web' }),
    }), scope, () => {});
    const data = await response.json();
    assert.equal(data.session_owner, 'desktop'); assert.equal('token' in data, false);
    assert.equal(stored, 'synthetic-private-session'); assert.equal(forwarded, null);
    assert.equal(scope.requests.size, 0);
  } finally { closeScope(scope); global.fetch = originalFetch; }
});

test('a late unauthorized response cannot clear a newer native session', async () => {
  const originalFetch = global.fetch;
  let token = 'old-session', revision = 1;
  const auth = { snapshot: () => ({ token, revision }), clear(snapshot) { if (snapshot.revision === revision && snapshot.token === token) token = ''; } };
  const scope = { origin: 'http://127.0.0.1:18193', host: 'service-race', requests: new Set(), closed: false, auth };
  global.fetch = async () => { token = 'new-session'; revision += 1; return Response.json({ error: 'unauthorized' }, { status: 401 }); };
  try {
    const response = await forwardService(new Request('muteki-desktop://service-race/api/threads'), scope, () => {});
    await response.text(); assert.equal(token, 'new-session');
  } finally { closeScope(scope); global.fetch = originalFetch; }
});

test('native remembered sessions are encrypted, installation-bound and removable', () => {
  const fs = require('node:fs'), os = require('node:os'), path = require('node:path'), crypto = require('node:crypto');
  const { ServiceAuthSessions } = require('../src/auth.cjs');
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'muteki-native-auth-'));
  const file = path.join(root, 'sessions.json'), key = crypto.randomBytes(32);
  // An encrypting fixture exercises the store's boundary, not Electron's OS keychain.
  const storage = { isEncryptionAvailable: () => true, getSelectedStorageBackend: () => 'gnome_libsecret',
    encryptString(value) { const iv = crypto.randomBytes(12), cipher = crypto.createCipheriv('aes-256-gcm', key, iv); const body = Buffer.concat([cipher.update(value, 'utf8'), cipher.final()]); return Buffer.concat([iv, cipher.getAuthTag(), body]); },
    decryptString(value) { const cipher = crypto.createDecipheriv('aes-256-gcm', key, value.subarray(0, 12)); cipher.setAuthTag(value.subarray(12, 28)); return Buffer.concat([cipher.update(value.subarray(28)), cipher.final()]).toString('utf8'); } };
  try {
    const sessions = new ServiceAuthSessions(file, storage), owner = sessions.forService('http://127.0.0.1:18193', 'installation-a');
    const data = { session_protocol: 2, session_owner: 'desktop', service_id: 'installation-a', auth_required: true, token: 'synthetic-private-token', expires_at: Date.now() / 1000 + 3600 };
    owner.accept(data, true);
    assert.equal(fs.readFileSync(file, 'utf8').includes(data.token), false);
    assert.equal(fs.statSync(file).mode & 0o777, 0o600);
    const restored = new ServiceAuthSessions(file, storage);
    assert.equal(restored.forService(owner.origin, owner.serviceId).snapshot().token, data.token);
    assert.equal(restored.forService(owner.origin, 'installation-b').snapshot().token, '');
    const old = owner.snapshot(); owner.accept({ ...data, token: 'new-private-token' }, false);
    owner.clear(old); assert.equal(owner.snapshot().token, 'new-private-token');
    assert.equal(new ServiceAuthSessions(file, storage).forService(owner.origin, owner.serviceId).snapshot().token, '');
    owner.clear(owner.snapshot()); assert.equal(owner.snapshot().token, '');
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

test('sidebar mode classification preserves run aliases and pentest query routes', () => {
  const { desktopRouteMode } = require('../src/policy.cjs');
  for (const route of ['/ctf', '/run/example?panel=files', '/run/example/collaboration', '/task/example']) assert.equal(desktopRouteMode(route), 'ctf');
  assert.equal(desktopRouteMode('/pentest?run=example'), 'pentest');
  for (const mode of ['chat', 'settings', 'competitions', 'usage']) assert.equal(desktopRouteMode('/' + mode), mode);
});

// Exercise the main-process navigation functions with a small Electron fixture.
// Keeping the real functions here covers cancellation and view ownership without
// starting the desktop app or replacing the user's running service.
function desktopNavigationFixture() {
  const fs = require('node:fs'), path = require('node:path'), vm = require('node:vm');
  const policy = require('../src/policy.cjs');
  const source = fs.readFileSync(path.join(__dirname, '../src/main.cjs'), 'utf8');
  const section = (start, end) => source.slice(source.indexOf(start), source.indexOf(end, source.indexOf(start)));
  const views = [], clientRequests = [];
  const remoteSession = { cookies: { set: async () => {} }, setPermissionCheckHandler() {}, setPermissionRequestHandler() {} };
  class View {
    constructor() {
      views.push(this); this.visible = false;
      const events = new Map();
      this.webContents = { url: '', closed: false, session: remoteSession,
        isDestroyed() { return this.closed; }, close() { this.closed = true; }, focus() {},
        getURL() { return this.url; }, getUserAgent: () => 'fixture', setUserAgent() {},
        on(name, handler) { events.set(name, handler); },
        emit(name, ...args) { events.get(name)?.(...args); },
        setWindowOpenHandler() {},
        async loadURL(url) { this.url = url; events.get('dom-ready')?.(); },
      };
    }
    setBorderRadius() {} setBounds() {}
    setVisible(value) { this.visible = value; }
  }
  const record = { state: { route: '/chat', configuring: false }, scope: { origin: 'http://localhost:3001', closed: false },
    window: { isDestroyed: () => false, getContentSize: () => [1000, 800], webContents: { focus() {} },
      contentView: { children: [], addChildView(view) { this.children.push(view); }, removeChildView(view) { this.children = this.children.filter(child => child !== view); } } } };
  const context = { ...policy, URL, Map, Set, safeRoute: policy.normalizeDesktopRoute,
    emit: (owner, patch) => Object.assign(owner.state, patch), persist() {}, disposePreview() {},
    setRemoteActive: async () => true, applyRemoteAppearance: async () => {}, applyRemoteChrome: async () => {},
    navigateRemoteClient: async (contents, href) => { clientRequests.push(href); contents.url = record.scope.origin + href; return { supported: true, href }; },
    WebContentsView: View, session: { fromPartition: () => remoteSession }, windows: new Map([[1, record]]),
    app: { getVersion: () => 'fixture' }, managedService: undefined, installKeyboard() {}, openExternal: async () => {}, transportError: error => error, SURFACE_RADIUS: 12,
    SURFACE_INSET: { left: 0, top: 0, right: 0, bottom: 0 },
    DesktopError: class extends Error { constructor(code, message) { super(message); this.code = code; } },
  };
  const functions = section('function resetNavigation(', 'async function executeCommand(')
    + section('function disposeRemoteView(', 'function handoffNotificationEntries(')
    + section('function remoteBounds(', 'async function setRemoteActive(')
    + section('async function parkRemote(', '// Resolves after React')
    + section('async function navigate(record,', 'const PREVIEW_LOG_LIMIT');
  const api = vm.runInNewContext(functions + '\n({navigate, resetNavigation, disposeRemote});', context);
  Object.assign(record.state, api.resetNavigation(record, '/chat'));
  return { ...api, record, views, clientRequests, context };
}

test('sidebar activation restores each cached document without navigation or scroll reset', async () => {
  const { navigate, record, views, clientRequests, disposeRemote } = desktopNavigationFixture();
  await navigate(record, '/run/example/collaboration?panel=files');
  const ctf = record.remote;
  ctf.scrollPosition = 427; ctf.selection = 'evidence';
  await navigate(record, '/pentest?run=second');
  const pentest = record.remote;
  await navigate(record, '/competitions/event');
  const competition = record.remote;
  await navigate(record, '/usage');
  const usage = record.remote;
  await navigate(record, '/chat/last-thread');
  assert.equal(record.remote, undefined);
  assert.equal(record.parkedRemotes.size, 4);
  assert.ok(views.every(view => !view.visible && !view.webContents.closed));
  await navigate(record, '/ctf', 'activate');
  assert.equal(record.remote, ctf);
  assert.equal(record.state.route, '/run/example/collaboration?panel=files');
  assert.equal(ctf.scrollPosition, 427); assert.equal(ctf.selection, 'evidence');
  await navigate(record, '/pentest', 'activate'); assert.equal(record.remote, pentest);
  assert.equal(record.state.route, '/pentest?run=second');
  await navigate(record, '/competitions', 'activate'); assert.equal(record.remote, competition);
  await navigate(record, '/usage', 'activate'); assert.equal(record.remote, usage);
  assert.deepEqual(clientRequests, []);
  assert.equal(views.length, 4);
  assert.equal(record.window.contentView.children.length, 1);
  assert.equal(views.filter(view => view.visible).length, 1);
  disposeRemote(record);
  assert.ok(views.every(view => view.webContents.closed));
  assert.equal(record.remote, undefined); assert.equal(record.parkedRemotes.size, 0);
});

test('explicit navigation and history traversal retain their requested routes', async () => {
  const { navigate, record, clientRequests, resetNavigation } = desktopNavigationFixture();
  await navigate(record, '/run/first');
  const ctf = record.remote;
  await navigate(record, '/chat/thread');
  await navigate(record, '/ctf', 'push');
  assert.equal(record.remote, ctf); assert.equal(record.state.route, '/ctf');
  assert.deepEqual(clientRequests, ['/ctf']);
  await navigate(record, '/run/second', 'replace');
  assert.equal(record.state.route, '/run/second');
  await navigate(record, '/chat/thread', 'activate');
  assert.equal(record.state.route, '/chat/thread');
  await navigate(record, '/run/first', 'traverse', 1);
  assert.equal(record.state.route, '/run/first'); assert.equal(record.navigation.index, 1);
  await navigate(record, '/settings/appearance');
  await navigate(record, '/chat', 'activate');
  assert.equal(record.state.route, '/chat/thread');
  await navigate(record, '/settings', 'activate');
  assert.equal(record.state.route, '/settings/appearance');
  Object.assign(record.state, resetNavigation(record, '/chat'));
  assert.equal(record.modeRoutes.has('ctf'), false);
});

test('cached views cannot move into another connection scope', async () => {
  const { navigate, record, views } = desktopNavigationFixture();
  await navigate(record, '/run/private'); const oldView = record.remote;
  await navigate(record, '/chat');
  record.scope = { ...record.scope };
  await navigate(record, '/ctf', 'activate');
  assert.notEqual(record.remote, oldView); assert.equal(oldView.webContents.closed, true);
  assert.equal(views.length, 2);
});

test('a newer navigation wins while an earlier mode is being parked', async () => {
  const { navigate, record, context, views } = desktopNavigationFixture();
  await navigate(record, '/run/first'); const ctf = record.remote;
  let release;
  context.setRemoteActive = (_view, active) => active ? Promise.resolve(true) : new Promise(resolve => { release = resolve; });
  const pending = navigate(record, '/pentest');
  await navigate(record, '/run/second');
  release(true); await pending;
  assert.equal(record.remote, ctf); assert.equal(record.state.route, '/run/second');
  assert.equal(record.navigationPending, false); assert.equal(views.length, 1);
});


test('a failed navigation closes only its document and keeps other modes usable', async () => {
  const { navigate, record, context } = desktopNavigationFixture();
  await navigate(record, '/run/first'); const ctf = record.remote;
  await navigate(record, '/pentest?run=second'); const pentest = record.remote;
  context.navigateRemoteClient = async () => { throw new Error('synthetic router failure'); };
  await assert.rejects(navigate(record, '/pentest?run=third'), { code: 'desktop.page_load_failed' });
  assert.equal(pentest.webContents.closed, true); assert.equal(ctf.webContents.closed, false);
  assert.equal(record.navigationPending, false);
  await navigate(record, '/ctf', 'activate');
  assert.equal(record.remote, ctf); assert.equal(record.state.route, '/run/first');
});


test('Pentest graph ownership survives mode activation and history traversal', async () => {
  const { navigate, record, clientRequests } = desktopNavigationFixture();
  const graphRoute = '/run/shared/collaboration';
  await navigate(record, graphRoute); const ctf = record.remote;
  await navigate(record, '/pentest?run=shared'); const pentest = record.remote;
  pentest.webContents.url = record.scope.origin + graphRoute;
  pentest.webContents.emit('did-navigate-in-page', {}, pentest.webContents.url, true);
  assert.equal(record.state.route, graphRoute); assert.equal(record.state.workspaceMode, 'pentest');
  assert.equal(record.modeRoutes.get('pentest'), graphRoute);
  const graphIndex = record.navigation.index;
  await navigate(record, '/ctf', 'activate');
  assert.equal(record.remote, ctf); assert.equal(record.state.workspaceMode, 'ctf');
  await navigate(record, '/pentest', 'activate');
  assert.equal(record.remote, pentest); assert.equal(record.state.workspaceMode, 'pentest');
  assert.equal(record.state.route, graphRoute); assert.deepEqual(clientRequests, []);
  await navigate(record, '/chat');
  await navigate(record, graphRoute, 'traverse', graphIndex);
  assert.equal(record.remote, pentest); assert.equal(record.state.workspaceMode, 'pentest');
  assert.equal(record.navigation.index, graphIndex);
});

test('cross-mode full navigation is intercepted before the old document unloads', async () => {
  const { navigate, record } = desktopNavigationFixture();
  await navigate(record, '/run/first'); const ctf = record.remote;
  let prevented = false;
  ctf.webContents.emit('will-navigate', { url: record.scope.origin + '/pentest?run=second', preventDefault() { prevented = true; } });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(prevented, true);
  assert.equal(ctf.webContents.getURL(), record.scope.origin + '/run/first');
  assert.equal(record.parkedRemotes.get('ctf').view, ctf);
  assert.equal(record.state.route, '/pentest?run=second');
  assert.equal(record.state.workspaceMode, 'pentest');
});


test('client ACK cannot reintroduce view keys already consumed by the live document', async () => {
  const { navigate, record, context } = desktopNavigationFixture();
  await navigate(record, '/pentest');
  context.navigateRemoteClient = async (contents, href) => {
    contents.url = record.scope.origin + '/pentest?run=selected';
    return { supported: true, href };
  };
  await navigate(record, '/pentest?run=selected&view=timeline');
  assert.equal(record.state.route, '/pentest?run=selected');
  assert.equal(record.modeRoutes.get('pentest'), '/pentest?run=selected');
});

test('parking snapshots query consumption that finished after the navigation ACK', async () => {
  const { navigate, record, clientRequests } = desktopNavigationFixture();
  await navigate(record, '/pentest?run=selected&view=timeline');
  const pentest = record.remote;
  // The component finishes its one-shot URL cleanup after native navigation.
  pentest.webContents.url = record.scope.origin + '/pentest?run=selected';
  await navigate(record, '/chat');
  assert.equal(record.modeRoutes.get('pentest'), '/pentest?run=selected');
  await navigate(record, '/pentest', 'activate');
  assert.equal(record.remote, pentest);
  assert.equal(record.state.route, '/pentest?run=selected');
  assert.deepEqual(clientRequests, []);
});
