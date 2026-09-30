const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { DEFAULT_ORIGIN, normalizeOrigin, normalizeDesktopRoute, allowedNavigation, partitionFor } = require('../src/policy.cjs');
const { readPreferences, writePreferences } = require('../src/preferences.cjs');

test('accepts HTTPS and local development origins, with canonical session identities', () => {
  assert.equal(normalizeOrigin(' https://MUTEKI.example:443/ '), 'https://muteki.example');
  assert.equal(normalizeOrigin('http://localhost:3001/'), 'http://localhost:3001');
  assert.equal(normalizeOrigin('http://[::1]:3001'), 'http://[::1]:3001');
  assert.equal(partitionFor('https://MUTEKI.example:443/'), partitionFor('https://muteki.example'));
  assert.notEqual(partitionFor(DEFAULT_ORIGIN), partitionFor('http://127.0.0.1:3002'));
});

test('rejects credential-bearing URLs, insecure remote hosts and non-web schemes', () => {
  for (const value of [null, {}, '', 'muteki.example', 'file:///etc/hosts', 'javascript:alert(1)',
    'https://user:password@muteki.example', 'https://muteki.example/?token=example',
    'https://muteki.example/#example', 'https://muteki.example/chat',
    'http://192.168.1.2:3001', 'http://localhost.evil.example', 'http://127.0.0.1.evil.example']) {
    assert.throws(() => normalizeOrigin(value));
  }
});

test('keeps service navigation on its origin and limits blob access to attachment previews', () => {
  const origin = 'https://muteki.example';
  assert.equal(allowedNavigation(`${origin}/chat/123`, origin), true);
  for (const value of ['https://other.example', 'https://muteki.example:444', 'file:///etc/hosts',
    'data:text/html,hello', 'javascript:alert(1)', 'https://user:password@muteki.example',
    'blob:https://other.example/abc']) {
    assert.equal(allowedNavigation(value, origin, { preview: true }), false, value);
  }
  assert.equal(allowedNavigation('about:blank', origin), false);
  assert.equal(allowedNavigation(`blob:${origin}/abc`, origin), false);
  assert.equal(allowedNavigation('about:blank', origin, { preview: true }), true);
  assert.equal(allowedNavigation(`blob:${origin}/abc`, origin, { preview: true }), true);
});

test('reports corrupt preferences without connecting an unrelated default service', () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'muteki-preferences-'));
  const file = path.join(directory, 'connection.json');
  try {
    assert.deepEqual(readPreferences(file), { origin: '', routes: {} });
    fs.writeFileSync(file, '{corrupt');
    assert.equal(readPreferences(file).error.code, 'desktop.preferences_invalid');
    writePreferences(file, 'https://muteki.example/');
    assert.deepEqual(JSON.parse(fs.readFileSync(file)), { origin: 'https://muteki.example', routes: {} });
    assert.throws(() => writePreferences(file, 'https://muteki.example/?token=example'));
    assert.deepEqual(readPreferences(file), { origin: 'https://muteki.example', routes: {} });
    assert.equal(fs.readdirSync(directory).some(name => name.endsWith('.tmp')), false);
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

test('validates encoded chat routes before they reach the renderer', () => {
  assert.equal(normalizeDesktopRoute('/chat/thread-%E4%B8%AD'), '/chat/thread-%E4%B8%AD');
  for (const route of ['/chat/thread-%', '/chat/thread-%E4']) {
    assert.throws(() => normalizeDesktopRoute(route), error => error.code === 'desktop.route_invalid');
  }
});
