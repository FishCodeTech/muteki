const { test } = require('node:test');
const assert = require('node:assert/strict');
const { normalizeRoute, activeRoute, iconForRoute } = require('../src/workspace-chrome.cjs');

test('desktop navigation stays on the selected service and excludes API documents', () => {
  for (const route of ['//other.example/chat', 'https://other.example', '/api/threads', '/_next/static/code.js', '/\\other.example', null]) assert.throws(() => normalizeRoute(route, 'https://muteki.example'));
  assert.equal(normalizeRoute('/chat/example?panel=files', 'https://muteki.example'), '/chat/example?panel=files');
});
test('deep links map to existing workspace entries without resetting their route', () => {
  const items = [{ href: '/chat' }, { href: '/task' }, { href: '/competitions' }, { href: '/custom/module' }];
  assert.equal(activeRoute('/run/example/collaboration', items, '/task'), '/task');
  assert.equal(activeRoute('/run/example/collaboration', items), '');
  const workspaces = [...items, { href: '/pentest' }];
  assert.equal(activeRoute('/run/example/collaboration', workspaces, '/pentest'), '/pentest');
  assert.equal(activeRoute('/run/example/collaboration', workspaces, '/unknown'), '');
  assert.equal(activeRoute('/chat/example', items), '/chat');
  assert.equal(activeRoute('/competitions/example', items), '/competitions');
  assert.equal(activeRoute('/settings/agents', items), 'settings');
  assert.equal(activeRoute('/custom/module/example', items), '/custom/module');
  assert.equal(iconForRoute('/custom/module'), 'extension');
});

const {
  itemsForSolveOnly,
  isRunWorkspaceHref,
  DEFAULT_WORKSPACE_ITEMS,
  READ_CHROME,
} = require('../src/workspace-chrome.cjs');
const { Script, createContext } = require('node:vm');

test('solve-only mode keeps only CTF and pentest rail entries', () => {
  const full = DEFAULT_WORKSPACE_ITEMS.map((item) => ({ ...item }));
  const solve = itemsForSolveOnly(true, full);
  assert.deepEqual(solve.map((item) => item.href), ['/ctf', '/pentest']);
  assert.equal(solve.every((item) => isRunWorkspaceHref(item.href)), true);
});

test('enabling full workspace mode restores chat and competition without a WorkspaceFrame', () => {
  const solveOnlyRail = [
    { href: '/ctf', label: 'CTF 工作台', badge: '1', icon: 'task' },
    { href: '/pentest', label: '渗透测试', badge: '', icon: 'pentest' },
  ];
  // Bug repro: settings page has no frame; sync must expand from solve-only rail.
  const full = itemsForSolveOnly(false, solveOnlyRail);
  assert.deepEqual(full.map((item) => item.href), ['/chat', '/ctf', '/pentest', '/competitions']);
  assert.equal(full.find((item) => item.href === '/ctf').badge, '1');
  // Reverse: turning solve-only back on drops chat/competition immediately.
  assert.deepEqual(itemsForSolveOnly(true, full).map((item) => item.href), ['/ctf', '/pentest']);
});

test('remembered full-mode rail wins over defaults when leaving settings', () => {
  const remembered = [
    { href: '/chat', label: '对话', badge: '', icon: 'chat' },
    { href: '/ctf', label: 'CTF', badge: '3', icon: 'task' },
    { href: '/pentest', label: '渗透', badge: '', icon: 'pentest' },
    { href: '/competitions', label: '比赛', badge: '2', icon: 'competition' },
    { href: '/custom/module', label: '扩展', badge: '', icon: 'extension' },
  ];
  assert.deepEqual(itemsForSolveOnly(false, remembered), remembered.map((item) => ({ ...item })));
  assert.deepEqual(itemsForSolveOnly(true, remembered).map((item) => item.href), ['/ctf', '/pentest']);
});

test('READ_CHROME reads persisted solveOnly when settings omit WorkspaceFrame', () => {
  const storage = { 'muteki.workspace.solveOnly': '0' };
  const sandbox = {
    location: { pathname: '/settings/appearance' },
    document: {
      documentElement: { dataset: { theme: 'light' } },
      querySelector: () => null,
      querySelectorAll: () => [],
    },
    localStorage: {
      getItem: (key) => (Object.prototype.hasOwnProperty.call(storage, key) ? storage[key] : null),
    },
    Array,
    URL,
    String,
  };
  createContext(sandbox);
  const chrome = new Script(READ_CHROME).runInContext(sandbox);
  assert.equal(chrome.hasFrame, false);
  assert.equal(chrome.solveOnly, false);
  assert.deepEqual(chrome.items, []);
  assert.equal(chrome.settingsHref, '/settings/agents');
  assert.equal(chrome.pathname, '/settings/appearance');

  storage['muteki.workspace.solveOnly'] = '1';
  const solveChrome = new Script(READ_CHROME).runInContext(sandbox);
  assert.equal(solveChrome.solveOnly, true);
  assert.equal(solveChrome.settingsHref, '/settings/appearance');
});

// Mirrors syncChrome's no-frame branch: toggle while staying on settings.
test('syncChrome no-frame branch updates rail from solveOnly without navigation', () => {
  let lastFullWorkspaceItems = [
    { href: '/chat', label: '对话', badge: '', icon: 'chat' },
    { href: '/ctf', label: 'CTF 工作台', badge: '', icon: 'task' },
    { href: '/pentest', label: '渗透测试', badge: '', icon: 'pentest' },
    { href: '/competitions', label: '比赛', badge: '', icon: 'competition' },
  ];
  let stateItems = lastFullWorkspaceItems.slice(0, 2).concat(lastFullWorkspaceItems.slice(2, 3)); // ctf+pentest only? use solve set
  stateItems = [
    { href: '/ctf', label: 'CTF 工作台', badge: '', icon: 'task' },
    { href: '/pentest', label: '渗透测试', badge: '', icon: 'pentest' },
  ];

  function apply(chrome) {
    let items = stateItems;
    if (chrome.hasFrame) {
      items = chrome.items;
      if (!chrome.solveOnly && items.length) lastFullWorkspaceItems = items;
    } else {
      items = itemsForSolveOnly(chrome.solveOnly, lastFullWorkspaceItems.length ? lastFullWorkspaceItems : stateItems);
    }
    const usage = chrome.hasFrame ? chrome.usage : !chrome.solveOnly;
    stateItems = items;
    return { items, usage, settingsHref: chrome.solveOnly ? '/settings/appearance' : '/settings/agents' };
  }

  // Stay on settings, turn ON full workspace mode.
  let next = apply({ hasFrame: false, solveOnly: false, usage: false });
  assert.deepEqual(next.items.map((item) => item.href), ['/chat', '/ctf', '/pentest', '/competitions']);
  assert.equal(next.usage, true);
  assert.equal(next.settingsHref, '/settings/agents');

  // Stay on settings, turn OFF (solve-only).
  next = apply({ hasFrame: false, solveOnly: true, usage: false });
  assert.deepEqual(next.items.map((item) => item.href), ['/ctf', '/pentest']);
  assert.equal(next.usage, false);
  assert.equal(next.settingsHref, '/settings/appearance');
});
