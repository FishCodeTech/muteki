const { test } = require('node:test');
const assert = require('node:assert/strict');
const { normalizeRoute, activeRoute, iconForRoute } = require('../src/workspace-chrome.cjs');

test('desktop navigation stays on the selected service and excludes API documents', () => {
  for (const route of ['//other.example/chat', 'https://other.example', '/api/threads', '/_next/static/code.js', '/\\other.example', null]) assert.throws(() => normalizeRoute(route, 'https://muteki.example'));
  assert.equal(normalizeRoute('/chat/example?panel=files', 'https://muteki.example'), '/chat/example?panel=files');
});
test('deep links map to existing workspace entries without resetting their route', () => {
  const items = [{ href: '/chat' }, { href: '/task' }, { href: '/competitions' }, { href: '/custom/module' }];
  assert.equal(activeRoute('/run/example/collaboration', items), '/task');
  assert.equal(activeRoute('/chat/example', items), '/chat');
  assert.equal(activeRoute('/competitions/example', items), '/competitions');
  assert.equal(activeRoute('/settings/agents', items), 'settings');
  assert.equal(activeRoute('/custom/module/example', items), '/custom/module');
  assert.equal(iconForRoute('/custom/module'), 'extension');
});
