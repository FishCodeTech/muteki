const { test } = require('node:test');
const assert = require('node:assert/strict');
const { normalizeDesktopRoute } = require('../src/policy.cjs');
const { forwardService, closeScope } = require('../src/transport.cjs');

test('chat and settings routes preserve their own path and query', () => {
  for (const route of ['/chat/example?panel=files', '/settings/appearance', '/settings/agent-extensions', '/settings/notifications']) assert.equal(normalizeDesktopRoute(route), route);
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
