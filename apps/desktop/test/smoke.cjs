const { _electron } = require('playwright-core');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { createFixture } = require('./fixture.cjs');

// Runtime inspection only. Actual interaction and visual QA use Computer Use.
(async () => {
  const directory = path.resolve(__dirname, '..');
  const fixture = await createFixture();
  const profile = await fs.mkdtemp(path.join(os.tmpdir(), 'muteki-local-chat-'));
  const executablePath = process.env.MUTEKI_DESKTOP_EXECUTABLE || require('electron');
  const env = { ...process.env, MUTEKI_DESKTOP_USER_DATA: profile, MUTEKI_DESKTOP_URL: fixture.origin, MUTEKI_DESKTOP_REGISTER_PROTOCOL: '0' };
  delete env.ELECTRON_RUN_AS_NODE;
  let application;
  try {
    application = await _electron.launch({ executablePath, args: process.env.MUTEKI_DESKTOP_EXECUTABLE ? [] : [directory], env });
    const page = await application.firstWindow();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.getByRole('textbox', { name: '输入消息', exact: true }).waitFor({ timeout: 20000 });
    const result = await application.evaluate(({ BrowserWindow, webContents }) => {
      const window = BrowserWindow.getAllWindows().find(item => item.webContents.getURL().startsWith('muteki-desktop://app/'));
      return { preferences: window.webContents.getLastWebPreferences(),
        views: window.contentView.children.length,
        remoteChat: webContents.getAllWebContents().some(item => /^https?:/.test(item.getURL()) && new URL(item.getURL()).pathname.startsWith('/chat')) };
    });
    assert.equal(result.preferences.sandbox, true);
    assert.equal(result.preferences.contextIsolation, true);
    assert.equal(result.preferences.nodeIntegration, false);
    assert.ok(result.preferences.preload);
    assert.equal(result.views, 0);
    assert.equal(result.remoteChat, false);
    assert.deepEqual(errors, []);
    console.log('PASS: shared chat renders in the local sandboxed window without a remote chat view');
  } finally {
    if (application) await application.close();
    await fixture.close();
    await fs.rm(profile, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
