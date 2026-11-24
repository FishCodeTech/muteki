#!/usr/bin/env node
import { spawn, spawnSync } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { createWebGateway } from './web-gateway.mjs';

const release = path.dirname(fileURLToPath(import.meta.url));
const action = process.argv[2] || 'web';
if (['--help','-h'].includes(action)) { console.log('Usage: muteki [web|version|status|install|upgrade|rollback] [options]'); process.exit(0); }
if (action !== 'web') {
  if (!['version','status','install','upgrade','rollback'].includes(action)) throw new Error(`Unknown command: ${action}`);
  const result=spawnSync(path.join(release,'bin/muteki-server'),process.argv.slice(2),{stdio:'inherit',env:{...process.env,MUTEKI_INSTALL_ROOT:process.env.MUTEKI_INSTALL_ROOT||path.resolve(release,'../..')}});
  if(result.error)throw result.error;process.exit(result.status??1);
}

const installRoot = process.env.MUTEKI_INSTALL_ROOT || path.resolve(release, '../..');
let install = {};
try { install = JSON.parse(await readFile(path.join(installRoot, 'install.json'), 'utf8')); } catch {}
const stateDir = process.env.MUTEKI_STATE_DIR || install.native_state_root || path.join(installRoot, 'data/state');
const apiPort = process.env.MUTEKI_API_PORT || '8100';
const uiPort = process.env.MUTEKI_UI_PORT || '3101';
const uiHost = process.env.MUTEKI_UI_HOST || '127.0.0.1';
const node = path.join(release, 'bin/node');
const server = path.join(release, 'bin/muteki-server');
const frontend = path.join(release, 'frontend/server.js');
const sharedEnv = {
  ...process.env,
  MUTEKI_INSTALL_ROOT: installRoot,
  MUTEKI_STATE_DIR: stateDir,
  MUTEKI_API_UPSTREAM: `http://127.0.0.1:${apiPort}`,
  NEXT_TELEMETRY_DISABLED: '1',
};

const gateway = await createWebGateway({ host: uiHost, port: uiPort, env: sharedEnv });
if (['127.0.0.1', '::1'].includes(uiHost)) sharedEnv.MUTEKI_UI_ORIGIN = gateway.origin;
const children = new Set();
let stopping = false;
function stop(code = 0) {
  if (stopping) return;
  stopping = true;
  process.exitCode = code;
  gateway.close();
  for (const child of children) child.kill('SIGTERM');
}
function launch(binary, args, options = {}) {
  const child = spawn(binary, args, { cwd: release, env: sharedEnv, stdio: 'inherit', ...options });
  children.add(child);
  child.on('error', error => { console.error(error.message); stop(1); });
  child.on('exit', code => { children.delete(child); if (!stopping) stop(code ?? 1); });
}
process.on('SIGINT', () => stop(130));
process.on('SIGTERM', () => stop(143));

launch(server, ['--host', '127.0.0.1', '--ui-host', uiHost, '--port', apiPort, '--state-dir', stateDir, ...process.argv.slice(3)]);
launch(node, [frontend], { env: { ...sharedEnv, HOSTNAME: '127.0.0.1', PORT: String(gateway.upstreamPort) } });
console.log(`Muteki Web starting at ${gateway.origin} (API 127.0.0.1:${apiPort})`);
