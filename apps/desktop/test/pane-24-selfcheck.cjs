#!/usr/bin/env node
/**
 * Self-check for FishCodeTech/muteki#24:
 * Toggling workspace display mode on a settings page (no WorkspaceFrame)
 * must immediately update the native rail config without navigation.
 */
const assert = require('node:assert/strict');
const { Script, createContext } = require('node:vm');
const {
  READ_CHROME,
  itemsForSolveOnly,
  DEFAULT_WORKSPACE_ITEMS,
} = require('../src/workspace-chrome.cjs');

function readChrome(pathname, solveOnlyValue) {
  const storage = { 'muteki.workspace.solveOnly': solveOnlyValue };
  const sandbox = {
    location: { pathname },
    document: {
      documentElement: { dataset: { theme: 'dark' } },
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
  return new Script(READ_CHROME).runInContext(sandbox);
}

function syncNoFrame(chrome, lastFull, stateItems) {
  const items = itemsForSolveOnly(chrome.solveOnly, lastFull.length ? lastFull : stateItems);
  return {
    items,
    usage: !chrome.solveOnly,
    settingsHref: chrome.settingsHref,
    active: 'settings',
  };
}

const lastFull = DEFAULT_WORKSPACE_ITEMS.map((item) => ({ ...item }));
let stateItems = itemsForSolveOnly(true, lastFull);
assert.deepEqual(stateItems.map((i) => i.href), ['/ctf', '/pentest'], 'start in CTF+pentest-only');

// Repro step 3: on settings appearance, turn ON "显示对话和比赛模式" (solveOnly=0).
const onSettingsFull = readChrome('/settings/appearance', '0');
assert.equal(onSettingsFull.hasFrame, false, 'settings has no WorkspaceFrame');
assert.equal(onSettingsFull.solveOnly, false, 'persisted mode is full workspace');
let next = syncNoFrame(onSettingsFull, lastFull, stateItems);
assert.deepEqual(next.items.map((i) => i.href), ['/chat', '/ctf', '/pentest', '/competitions']);
assert.equal(next.usage, true);
assert.equal(next.settingsHref, '/settings/agents');
assert.equal(next.active, 'settings', 'stay on settings — no navigate-home required');
stateItems = next.items;

// Reverse: turn OFF while still on settings.
const onSettingsSolve = readChrome('/settings/appearance', '1');
assert.equal(onSettingsSolve.hasFrame, false);
assert.equal(onSettingsSolve.solveOnly, true);
next = syncNoFrame(onSettingsSolve, lastFull, stateItems);
assert.deepEqual(next.items.map((i) => i.href), ['/ctf', '/pentest']);
assert.equal(next.usage, false);
assert.equal(next.settingsHref, '/settings/appearance');
assert.equal(next.active, 'settings');

console.log('PASS: pane-24 desktop chrome sync — settings toggle updates native rail without leaving settings');
