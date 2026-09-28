const { _electron } = require('playwright-core');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { createFixture } = require('./fixture.cjs');

// Read-only runtime assertions. Interactive navigation and visual comparison
// are performed with Computer Use against this disposable Web fixture.
(async () => {
  const directory = path.resolve(__dirname, '..'), output = path.join(directory, '.test-output');
  const fixture = await createFixture();
  const profile = await fs.mkdtemp(path.join(os.tmpdir(), 'muteki-navigation-'));
  const executablePath = process.env.MUTEKI_DESKTOP_EXECUTABLE || require('electron');
  const env = { ...process.env, MUTEKI_DESKTOP_USER_DATA: profile, MUTEKI_DESKTOP_URL: fixture.origin }; delete env.ELECTRON_RUN_AS_NODE;
  let application;
  try {
    await fs.mkdir(output, { recursive: true });
    application = await _electron.launch({ executablePath, args: process.env.MUTEKI_DESKTOP_EXECUTABLE ? [] : [directory], env });
    const page = await application.firstWindow();
    const errors = []; page.on('pageerror', error => errors.push(error.message));
    await page.getByRole('button', { name: '全局用量', exact: true }).waitFor({ timeout: 15000 });
    const result = await application.evaluate(async ({ webContents, BrowserWindow }) => {
      const remote = webContents.getAllWebContents().find(contents => contents.getURL().startsWith('http://127.0.0.1:'));
      const main = BrowserWindow.getAllWindows()[0];
      const view = main.contentView.children.find(child => child.webContents === remote);
      return {
        prefs: remote.getLastWebPreferences(), bounds: view.getBounds(), size: main.getContentSize(),
        page: await remote.executeJavaScript(`({nav: getComputedStyle(document.querySelector('.workspace-nav')).display, original: !!document.querySelector('[data-original-business]'), composer: !!document.querySelector('#original-composer'), require: typeof require, bridge: typeof mutekiDesktop})`),
      };
    });
    assert.equal(result.page.nav, 'none'); assert.ok(result.page.original && result.page.composer);
    assert.equal(result.page.require, 'undefined'); assert.equal(result.page.bridge, 'undefined');
    assert.ok(result.prefs.sandbox && result.prefs.contextIsolation && !result.prefs.nodeIntegration && !result.prefs.preload);
    assert.equal(result.bounds.x, 56); assert.equal(result.bounds.y, 44);
    assert.equal(result.bounds.width, result.size[0] - 56); assert.equal(result.bounds.height, result.size[1] - 44);
    async function capture() {
      const images = await application.evaluate(async ({ BrowserWindow, webContents }) => {
        const main = BrowserWindow.getAllWindows().find(win => win.webContents.getURL().startsWith('muteki-desktop:'));
        const remote = webContents.getAllWebContents().find(contents => contents.getURL().startsWith('http://127.0.0.1:'));
        const shell = await main.capturePage(), page = await remote.capturePage();
        return { shell: shell.toPNG().toString('base64'), page: page.toPNG().toString('base64'), shellSize: shell.getSize(), pageSize: page.getSize(), contentSize: main.getContentSize() };
      });
      await fs.writeFile(path.join(output, 'navigation-shell.png'), Buffer.from(images.shell, 'base64'));
      await fs.writeFile(path.join(output, 'navigation-page.png'), Buffer.from(images.page, 'base64'));
      await fs.writeFile(path.join(output, 'navigation-capture.json'), JSON.stringify({ shellSize: images.shellSize, pageSize: images.pageSize, contentSize: images.contentSize }));
    }
    await capture();
    if (process.env.QA_CAPTURE) {
      console.log(`Desktop navigation QA is open. Fixture: ${fixture.origin}`);
      let stop; const done = new Promise(resolve => { stop = resolve; });
      const timer = setInterval(() => { void capture().catch(() => stop()); }, 1600);
      application.on('close', stop); process.once('SIGTERM', stop); process.once('SIGINT', stop);
      await done; clearInterval(timer);
    }
    assert.deepEqual(errors, []);
    console.log('PASS: original business DOM preserved, only global nav hidden, remote renderer isolated, native view bounds fit');
  } finally {
    if (application) await application.close().catch(() => {});
    await fixture.close(); await fs.rm(profile, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
