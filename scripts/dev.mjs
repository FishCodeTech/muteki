#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { createWebGateway } from './web-gateway.mjs';
import { spawn } from 'node:child_process';
import { request } from 'node:http';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { existsSync } from 'node:fs';
import { buildFrontend, cargo } from './build-steps.mjs';
const root = fileURLToPath(new URL('../', import.meta.url));
const argv = process.argv.slice(2);
let uiHost = '127.0.0.1';
const hostIndex = argv.findIndex(arg => arg === '--host' || arg.startsWith('--host='));
if (hostIndex >= 0) {
  const inline = argv[hostIndex].startsWith('--host=');
  uiHost = inline ? argv[hostIndex].slice('--host='.length) : argv[hostIndex + 1];
  argv.splice(hostIndex, inline ? 1 : 2);
  if (!uiHost || !/^[0-9a-fA-F:.]+$/.test(uiHost)) throw new Error('--host needs an IP address, for example 0.0.0.0 or 192.168.1.20.');
}
const args = new Set(argv);
if (args.has('--help')) {
  console.log('Usage: node scripts/dev.mjs [--web] [--hot] [--host ADDRESS] [--release] [--skip-build]\n--hot: live frontend updates on port 3103, reusing existing Rust binaries (built once if missing); no frontend build or app packaging.\nDefault: builds Rust binaries and the original Next frontend, then opens NSH.\n--web: serves the same frontend at http://127.0.0.1:3101.\n--host: with --web, serves the UI on ADDRESS (for example 0.0.0.0 for the LAN). A non-loopback address requires an access password from Settings or MUTEKI_WEB_PASSWORD; the backend stays on loopback.\n--release: uses the Cargo "release" profile (.cache/target/release); default uses "local" (optimized, incremental, no LTO).\n--skip-build: uses existing Rust binaries and Next production build.\nRust and the Next build run in parallel; an unchanged frontend reuses its last build (MUTEKI_FRONTEND_REBUILD=1 forces it).\nState defaults to workspace .state; MUTEKI_STATE_DIR overrides it.');
  process.exit(0);
}
if ([...args].some(arg => !['--web', '--hot', '--release', '--skip-build'].includes(arg))) throw new Error('Unknown argument. Use --help.');
const loopbackUi = ['127.0.0.1', '::1'].includes(uiHost);
if (!loopbackUi && !args.has('--web')) throw new Error('--host applies to --web only; the desktop app always stays on loopback.');
const profile = args.has('--release') ? 'release' : 'local';
const cargoProfile = ['--profile', profile];
const frontend = path.join(root, 'apps/frontend');
const env = { ...process.env, MUTEKI_NODE: process.execPath, MUTEKI_API_UPSTREAM: 'http://127.0.0.1:8100', NEXT_TELEMETRY_DISABLED: '1', MUTEKI_STATE_DIR: process.env.MUTEKI_STATE_DIR ?? path.join(root, '.state') };
env.MUTEKI_UI_HOST = uiHost;
const gateway = args.has('--web') && !args.has('--hot') ? await createWebGateway({ host: uiHost, port: 3101, env }) : null;
if (gateway && loopbackUi) env.MUTEKI_UI_ORIGIN = gateway.origin;
const children = new Set();
let stopping = false;
function stop(code = 0) {
  if (stopping) return;
  stopping = true;
  process.exitCode = code;
  gateway?.close();
  for (const child of children) child.kill('SIGTERM');
}
process.on('SIGINT', () => stop(130));
process.on('SIGTERM', () => stop(143));
function launch(command, arguments_) {
  const child = spawn(command, arguments_, { cwd: root, env, stdio: 'inherit' });
  children.add(child);
  child.on('error', error => { console.error(error.message); stop(1); });
  child.on('exit', code => { children.delete(child); if (!stopping) stop(code ?? 1); });
  return child;
}
async function exposureAllowed() {
  const response = await fetch('http://127.0.0.1:8100/api/auth/status', { signal: AbortSignal.timeout(2000) });
  const status = await response.json();
  return status.auth_required === true && status.exposed === true;
}
async function health() {
  return new Promise((resolve, reject) => {
    const req = request('http://127.0.0.1:8100/api/health', { timeout: 2000 }, response => {
      let body = '';
      response.setEncoding('utf8');
      response.on('data', chunk => { body += chunk; });
      response.on('end', () => {
        try {
          if (response.statusCode !== 200 || JSON.parse(body).app !== 'muteki-rust') throw new Error('Port 8100 is not the Muteki Rust service.');
          resolve(true);
        } catch (error) { reject(error); }
      });
    });
    req.on('timeout', () => req.destroy(new Error('Timed out verifying Muteki service.')));
    req.on('error', error => error.code === 'ECONNREFUSED' ? resolve(false) : reject(error));
    req.end();
  });
}
async function hotPreview() {
  if (!loopbackUi) throw new Error('--hot supports local preview only.');
  const origin = 'http://127.0.0.1:3103';
  env.MUTEKI_UI_ORIGIN = origin;
  const { createServer } = await import('node:net');
  await new Promise((resolve, reject) => {
    const probe = createServer();
    probe.once('error', () => reject(new Error('Preview port 3103 is occupied. Stop the previous preview command first.')));
    probe.listen(3103, '127.0.0.1', () => probe.close(resolve));
  });
  const binaries = ['muteki-server', ...(args.has('--web') ? [] : ['muteki-desktop'])];
  if (binaries.some(name => !existsSync(path.join(root, '.cache/target', profile, name)))) {
    if (args.has('--skip-build')) throw new Error('Missing Rust binaries. Run --hot without --skip-build once.');
    await cargo(['build', '--workspace', ...cargoProfile], env);
  }
  if (!(await health())) {
    launch(path.join(root, '.cache/target', profile, 'muteki-server'), ['--state-dir', env.MUTEKI_STATE_DIR]);
    const deadline = Date.now() + 15000;
    while (!(await health())) {
      if (stopping || Date.now() > deadline) throw new Error('Backend failed to start.');
      await new Promise(resolve => setTimeout(resolve, 100));
    }
  }
  env.MUTEKI_NEXT_DIST_DIR = '.next-dev';
  env.NODE_ENV = 'development';
  launch(process.execPath, [path.join(frontend, 'node_modules/next/dist/bin/next'), 'dev', frontend, '-H', '127.0.0.1', '-p', '3103']);
  const deadline = Date.now() + 120000;
  while (true) {
    if (stopping || Date.now() > deadline) throw new Error('Frontend preview failed to become ready.');
    const ready = await fetch(origin, { signal: AbortSignal.timeout(10000) }).catch(() => null);
    if (ready?.ok && (await ready.text()).includes('/_next/')) {
      const proxy = await fetch(`${origin}/api/health`, { signal: AbortSignal.timeout(10000) });
      if (!proxy.ok || (await proxy.json()).app !== 'muteki-rust') throw new Error('Preview API proxy is not connected to Muteki.');
      break;
    }
    await new Promise(resolve => setTimeout(resolve, 300));
  }
  if (!args.has('--web')) launch(path.join(root, '.cache/target', profile, 'muteki-desktop'), []);
  console.log(`Hot preview ready at ${origin}. TSX/CSS edits update automatically. Ctrl+C stops this preview's services.`);
}
try {
  if (args.has('--hot')) {
    await hotPreview();
  } else {
  if (!args.has('--skip-build')) {
    const builds = await Promise.allSettled([cargo(['build', '--workspace', ...cargoProfile], env), buildFrontend({ distDir: env.MUTEKI_NEXT_DIST_DIR || '.next', env })]);
    const failed = builds.find(build => build.status === 'rejected');
    if (failed) throw failed.reason;
  }
  if (!args.has('--web')) {
    launch(path.join(root, '.cache/target', profile, 'muteki-desktop'), []);
  } else {
    if (!(await health())) {
      const server = launch(path.join(root, '.cache/target', profile, 'muteki-server'), ['--state-dir', env.MUTEKI_STATE_DIR, '--ui-host', uiHost]);
      const deadline = Date.now() + 15000;
      while (!(await health())) {
        if (stopping || server.exitCode !== null) throw new Error('Muteki backend exited during startup.');
        if (Date.now() > deadline) throw new Error('Muteki backend did not become ready within 15 seconds.');
        await new Promise(resolve => setTimeout(resolve, 100));
      }
    }
    const identity = await (await fetch('http://127.0.0.1:8100/api/auth/status')).json();
    if (identity.proxy_key_fingerprint !== createHash('sha256').update(env.MUTEKI_WEB_PROXY_KEY).digest('hex')) {
      throw new Error('The running backend belongs to another Web ingress. Restart its owning dev command before starting this Web session.');
    }
    if (!loopbackUi && !(await exposureAllowed())) throw new Error(`Serving the UI on ${uiHost} requires an access password and a backend started for an exposed UI. Set a password in Settings or MUTEKI_WEB_PASSWORD, stop the running backend, and start again with --host.`);
    const nextDistDir = env.MUTEKI_NEXT_DIST_DIR || '.next';
    if (!existsSync(path.join(frontend, nextDistDir, 'BUILD_ID'))) throw new Error('Missing Next production build. Run without --skip-build.');
    launch(process.execPath, [path.join(frontend, 'node_modules/next/dist/bin/next'), 'start', frontend, '-H', '127.0.0.1', '-p', String(gateway.upstreamPort)]);
    console.log(`Muteki Web starting at ${gateway.origin}`);
  }
  }
} catch (error) {
  console.error(`Cannot start Muteki: ${error.message}`);
  stop(1);
}
