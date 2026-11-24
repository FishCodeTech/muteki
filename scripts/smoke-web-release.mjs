#!/usr/bin/env node
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { createServer } from 'node:net';
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const archive = path.resolve(process.argv[2] || '');
if (!archive.endsWith('.tar.gz')) throw new Error('Usage: node scripts/smoke-web-release.mjs <archive.tar.gz>');
const root = await mkdtemp('/tmp/mw-');
const logs = path.resolve('.cache/ci-logs');
await mkdir(logs, { recursive: true });
let child;
let output = '';
let launchError;
async function port() {
  const server = createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const value = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return value;
}
try {
  const unpack = spawnSync('tar', ['-xzf', archive, '-C', root], { encoding: 'utf8' });
  if (unpack.status !== 0) throw new Error(unpack.stderr || 'Archive extraction failed');
  const bundle = path.join(root, (await readdir(root)).find(name => name.startsWith('muteki-web-')));
  const manifest = JSON.parse(await readFile(path.join(bundle, 'release.json'), 'utf8'));
  const apiPort = await port();
  let uiPort = await port();
  while (uiPort === apiPort) uiPort = await port();
  const env = { ...process.env, MUTEKI_STATE_DIR: path.join(root, 'state'), MUTEKI_INSTALL_ROOT: path.join(root, 'install'),
    MUTEKI_API_PORT: String(apiPort), MUTEKI_UI_PORT: String(uiPort), MUTEKI_UI_HOST: '127.0.0.1', MUTEKI_WEB_PASSWORD: 'ci-package-smoke' };
  delete env.MUTEKI_RUNNER_SOCKET;
  child = spawn(path.join(bundle, 'bin/node'), [path.join(bundle, 'launcher.mjs'), 'web'], { env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  child.stdout.on('data', data => { output += data; });
  child.stderr.on('data', data => { output += data; });
  child.on('error', error => { launchError = error; output += error.message; });
  const deadline = Date.now() + 90_000;
  let ready = false;
  let token;
  let stage = 'login';
  let lastProbe = 'no response';
  while (Date.now() < deadline) {
    if (launchError) throw launchError;
    if (child.exitCode !== null || child.signalCode !== null) throw new Error('Packaged launcher exited before readiness');
    try {
      if (!token) {
        stage = 'login';
        const login = await fetch(`http://127.0.0.1:${uiPort}/api/auth/login`, {
          method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Muteki-CSRF': '1' },
          body: JSON.stringify({ password: 'ci-package-smoke', client: 'api' }), signal: AbortSignal.timeout(2000),
        });
        lastProbe = `login HTTP ${login.status}`;
        if (login.ok) {
          token = (await login.json())?.token;
          if (typeof token !== 'string' || !token) {
            token = undefined;
            lastProbe = 'login response missing token';
          }
        }
      }
      if (token) {
        stage = 'readiness';
        const response = await fetch(`http://127.0.0.1:${uiPort}/api/readiness`, {
          headers: { Authorization: `Bearer ${token}` }, signal: AbortSignal.timeout(2000),
        });
        lastProbe = `readiness HTTP ${response.status}`;
        if (response.status === 401) token = undefined;
        if (response.ok) {
          const health = await response.json();
          ready = health?.app === 'muteki-rust' && health.version === manifest.version && health.ready === true;
          if (ready) break;
          lastProbe = 'readiness response did not confirm app, version and ready state';
        }
      }
    } catch (error) {
      const kind = error instanceof SyntaxError ? 'invalid JSON' :
        error?.name === 'TimeoutError' ? 'request timed out' : 'request failed';
      lastProbe = `${stage}: ${kind}`;
    }
    await delay(500);
  }
  if (!ready) throw new Error(`Packaged API readiness failed: ${lastProbe}`);
  const page = await fetch(`http://127.0.0.1:${uiPort}`, { signal: AbortSignal.timeout(10_000) });
  const html = await page.text();
  const asset = html.match(/(?:src|href)="(\/_next\/static\/[^"?]+\.js(?:\?[^\"]*)?)"/)?.[1];
  if (!page.ok || !html.toLowerCase().includes('muteki') || !asset) throw new Error('Packaged UI or its static asset reference is missing');
  const script = await fetch(`http://127.0.0.1:${uiPort}${asset.replaceAll('&amp;', '&')}`, { signal: AbortSignal.timeout(10_000) });
  if (!script.ok || !(await script.text()).length) throw new Error('Packaged UI JavaScript could not be served');
  console.log(`WEB_RELEASE_OK ${manifest.platform}/${manifest.arch} ${manifest.version}: bundled launcher, API readiness, UI and JavaScript`);
} catch (error) {
  console.error(output);
  throw error;
} finally {
  if (child?.pid) {
    const exited = once(child, 'exit').catch(() => {});
    try { process.kill(-child.pid, 'SIGTERM'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
    await Promise.race([exited, delay(5000)]);
    try { process.kill(-child.pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
  }
  await writeFile(path.join(logs, 'web-release.log'), output);
  await rm(root, { recursive: true, force: true });
}
