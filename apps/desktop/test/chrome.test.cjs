const { test } = require('node:test');
const assert = require('node:assert/strict');
const { normalizeDesktopRoute } = require('../src/policy.cjs');
const { forwardService, closeScope } = require('../src/transport.cjs');

test('chat, run and settings routes preserve their own path and query', () => {
  for (const route of ['/chat/example?panel=files', '/run/run-123?panel=evidence', '/settings/appearance', '/settings/agent-extensions', '/settings/notifications']) assert.equal(normalizeDesktopRoute(route), route);
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
