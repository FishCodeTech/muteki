import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const [image, probe, traverse] = process.argv.slice(2);
if (!image || !probe || !traverse) {
  throw new Error('Usage: smoke.mjs HEADLESS_IMAGE PAIR_AND_QUERY_BINARY TRAVERSE_IMAGE');
}
const id = `tcode-smoke-${randomUUID()}`;
const data = `${id}-data`;
const projects = `${id}-projects`;
const traverseName = `${id}-traverse`;
const traverseData = `${id}-traverse-data`;
const device = await mkdtemp(path.join(os.tmpdir(), 'tcode-container-device-'));
const password = randomUUID();
// The invitation names the machine's UDP port, so the host port must match it;
// 47420 itself is taken wherever a Tcode desktop app is running.
const udpPort = 47520;
const commands = ['claude', 'codex', 'pi', 'opencode', 'cursor-agent', 'grok'];
const protocol = Number((await readFile(new URL('../../protocol/src/lib.rs', import.meta.url), 'utf8'))
  .match(/pub const PROTOCOL_VERSION: u32 = (\d+)/)[1]);

function command(program, args, options = {}) {
  const result = spawnSync(program, args, { encoding: 'utf8', timeout: 180_000, windowsHide: true, ...options });
  if (result.error) throw result.error;
  assert.equal(result.status, 0, `${program} ${args.join(' ')}\n${result.stdout}\n${result.stderr}`);
  return result.stdout.trim();
}
const docker = (...args) => command('docker', args);
const exec = script => docker('exec', '--user', 'tcode', id, 'sh', '-ec', script);
const jsonFile = file => JSON.parse(exec(`cat '${file}'`));

/// `agents` undefined leaves TCODE_AGENTS unset: every agent.
async function start(agents, uid = '1234', gid = '2345', dockerGid) {
  docker('run', '--detach', '--name', id,
    '--group-add', '3456',
    ...(dockerGid === undefined ? [] : ['--env', `DOCKER_GID=${dockerGid}`]),
    ...(agents === undefined ? [] : ['--env', `TCODE_AGENTS=${agents}`]),
    '--env', `PUID=${uid}`, '--env', `PGID=${gid}`, '--env', `TCODE_PASSWORD=${password}`, '--env', 'TZ=Asia/Shanghai',
    '--volume', `${data}:/data`,
    '--mount', `type=volume,src=${projects},dst=/projects,volume-nocopy`,
    '--publish', '127.0.0.1::47420/tcp', '--publish', `127.0.0.1:${udpPort}:${udpPort}/udp`,
    image, 'serve', '--traverse', 'off', '--browser-listen', '0.0.0.0:47420', '--port', String(udpPort));
  const port = JSON.parse(docker('inspect', id))[0].NetworkSettings.Ports['47420/tcp'][0].HostPort;
  const base = `http://127.0.0.1:${port}`;
  assert.deepEqual(await waitForJson(id, `${base}/auth/state`), { mode: 'password', configured: true });
  return base;
}

async function waitForJson(name, url) {
  for (let attempt = 0; attempt < 600; attempt++) {
    let response;
    try {
      response = await fetch(url, { signal: AbortSignal.timeout(1000) });
    } catch { /* Installation precedes the listener on a fresh volume. */ }
    if (response?.ok) return response.json();
    if (!JSON.parse(docker('inspect', name))[0].State.Running) throw new Error(docker('logs', name));
    await delay(1000);
  }
  throw new Error(`Container did not become ready:\n${docker('logs', name)}`);
}

async function waitForHealthy() {
  for (let attempt = 0; attempt < 60; attempt++) {
    const health = JSON.parse(docker('inspect', id))[0].State.Health?.Status;
    if (health === 'healthy') return;
    assert.notEqual(health, 'unhealthy', docker('logs', id));
    await delay(1000);
  }
  throw new Error('The image health check never passed');
}

function stop() {
  docker('stop', '--time', '30', id);
  assert.equal(JSON.parse(docker('inspect', id))[0].State.ExitCode, 0, docker('logs', id));
  docker('rm', id);
}

async function browser(base) {
  const page = await fetch(base);
  assert.equal(page.status, 200);
  assert.match(await page.text(), /auth\.mjs/);
  const auth = await fetch(`${base}/auth.mjs`);
  assert.equal(auth.status, 200);
  const authFile = path.join(device, 'auth.mjs');
  await writeFile(authFile, await auth.text());
  command(process.execPath, ['--check', authFile]);
  assert.doesNotMatch(await readFile(authFile, 'utf8'), /__AUTH_LOCALES__/, 'The login page embeds its translations');
  const wasm = await fetch(`${base}/tcode_web_bg.wasm`);
  assert.equal(wasm.status, 200);
  assert.deepEqual([...new Uint8Array(await wasm.arrayBuffer()).slice(0, 4)], [0, 97, 115, 109]);
  const login = async value => fetch(`${base}/auth/login`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ password: value, device_name: 'Container smoke' }),
  });
  assert.equal((await login('incorrect-password')).status, 403);
  const response = await login(password);
  assert.equal(response.status, 200);
  const { token } = await response.json();
  await connectBrowser(base, token);
  return token;
}

async function connectBrowser(base, token) {
  await new Promise((resolve, reject) => {
    const socket = new WebSocket(base.replace('http:', 'ws:') + '/ws');
    const timeout = setTimeout(() => { socket.close(); reject(new Error('Browser handshake timed out')); }, 10_000);
    socket.addEventListener('open', () => socket.send(JSON.stringify({
      type: 'hello', protocol_version: protocol, token, device: { name: 'Container smoke' },
    })));
    socket.addEventListener('error', event => { clearTimeout(timeout); reject(event); });
    socket.addEventListener('message', event => {
      try {
        assert.equal(JSON.parse(event.data).type, 'hello_ok');
        clearTimeout(timeout);
        socket.close();
        resolve();
      } catch (error) { clearTimeout(timeout); socket.close(); reject(error); }
    }, { once: true });
  });
}

function native(argument) {
  command(probe, [argument, ...(argument === '--connect' ? [] : ['127.0.0.1'])], {
    env: { ...process.env, TMPDIR: device, TEMP: device, TMP: device },
  });
}

try {
  docker('volume', 'create', data);
  docker('volume', 'create', projects);
  docker('run', '--rm', '--entrypoint', 'sh',
    '--mount', `type=volume,src=${projects},dst=/projects,volume-nocopy`, image,
    '-ec', 'chown 1234:2345 /projects');
  console.log('First start: every CLI installed, browser password, LAN pairing, project ownership and terminal PATH');
  let base = await start(undefined);
  for (const name of commands) exec(`${name} --version`);
  // The npm CLI runs from the global prefix, where npm's own updates land.
  const [uid, codex, globalCodex] = exec('bash -lc "id -u; command -v codex; echo $(npm prefix --global)/bin/codex"').split('\n');
  assert.deepEqual([uid, codex], ['1234', globalCodex]);
  assert.equal(exec('bash -lc "locale charmap; date +%Z"'), 'UTF-8\nCST', 'Terminal panes get UTF-8 and TZ');
  await waitForHealthy();
  exec('echo owned > /projects/owned');
  assert.equal(exec('stat -c "%u:%g" /projects/owned'), '1234:2345');
  const groups = exec('grep "^Groups:" /proc/$(pgrep -x tcode-headless)/status').split(/\s+/).slice(1).map(Number);
  assert(groups.includes(3456), 'Explicit supplementary groups reach the host process');
  assert(!groups.includes(0), 'The host process drops the root group');
  const token = await browser(base);
  const machineId = docker('logs', id).match(/Machine id: ([a-f0-9]+)/)[1];
  const invitation = docker('logs', id).match(/tcode:\/\/pair\?[^\s]+/)[0];
  native(invitation);
  exec('mkdir -p "$HOME/.codex"; printf persisted > "$HOME/.codex/container-smoke-marker"');
  exec('printf "#!/bin/sh\\necho acp-ok\\n" > /data/bin/custom-acp; chmod +x /data/bin/custom-acp');
  stop();

  console.log('Subset: omitted CLIs leave PATH and their providers, pairings and homes survive');
  base = await start('codex,pi');
  for (const name of ['claude', 'opencode', 'cursor-agent', 'grok']) exec(`! command -v ${name}`);
  const settings = jsonFile('/data/tcode/settings.json');
  for (const name of ['claude', 'opencode', 'cursor', 'grok']) assert.equal(settings.providers[name].enabled, false);
  assert.equal(settings.providers.codex.enabled, true);
  assert.equal(settings.providers.pi.enabled, true);
  assert.equal(docker('logs', id).match(/Machine id: ([a-f0-9]+)/)[1], machineId);
  assert.equal(exec('cat "$HOME/.codex/container-smoke-marker"'), 'persisted');
  native('--connect');
  await connectBrowser(base, token);
  stop();

  console.log('Installed CLIs are reused with networking disabled');
  docker('run', '--rm', '--network', 'none', '--entrypoint', 'sh', '--env', 'TCODE_AGENTS=codex',
    '--volume', `${data}:/data`, image, '-ec',
    'node /opt/tcode/install-agents.mjs /data/agents/bin; codex --version');

  console.log('Empty selection, custom ACP command, changed UID/GID, same identity');
  base = await start('', '1235', '2346', '0');
  for (const name of commands) exec(`! command -v ${name}`);
  assert.equal(exec('custom-acp'), 'acp-ok');
  assert.equal(exec('stat -c "%u:%g" /data/home/.codex/container-smoke-marker'), '1235:2346');
  assert(exec('grep "^Groups:" /proc/$(pgrep -x tcode-headless)/status').split(/\s+/).includes('0'),
    'A root-owned Docker socket group is granted only when explicitly configured');
  assert.equal(docker('logs', id).match(/Machine id: ([a-f0-9]+)/)[1], machineId);
  native('--connect');
  await connectBrowser(base, token);
  stop();

  console.log('Traverse image: HTTP manifest, data volume and graceful Docker stop');
  docker('run', '--detach', '--name', traverseName, '--volume', `${traverseData}:/var/lib/tcode-traverse`,
    '--publish', '127.0.0.1::8080', traverse,
    '--dev', '--http-bind', '0.0.0.0:8080', '--data-dir', '/var/lib/tcode-traverse');
  const traversePort = JSON.parse(docker('inspect', traverseName))[0].NetworkSettings.Ports['8080/tcp'][0].HostPort;
  const manifest = await waitForJson(traverseName, `http://127.0.0.1:${traversePort}/relays.json`);
  assert.equal(manifest.version, 1);
  assert.equal(manifest.relays.length, 1);
  assert.equal(manifest.pkarr.length, 1);
  docker('stop', '--time', '10', traverseName);
  assert.equal(JSON.parse(docker('inspect', traverseName))[0].State.ExitCode, 0, docker('logs', traverseName));
  docker('rm', traverseName);
  console.log('Container smoke checks passed (credential-file persistence; no account login performed).');
} finally {
  spawnSync('docker', ['rm', '--force', id, traverseName], { stdio: 'ignore', windowsHide: true });
  spawnSync('docker', ['volume', 'rm', data, projects, traverseData], { stdio: 'ignore', windowsHide: true });
  await rm(device, { recursive: true, force: true });
}
