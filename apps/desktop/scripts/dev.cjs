#!/usr/bin/env node
// Machine-readable Dev launcher/control. Tokens stay in the private profile.
const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { desktopEnvironment } = require('../src/environment.cjs');

async function main() {
  const [command, ...args] = process.argv.slice(2);
  const option = name => { const index = args.indexOf(`--${name}`); return index < 0 ? undefined : args[index + 1]; };
  if (!['start', 'stop', 'status', 'snapshot', 'screenshot', 'click', 'type'].includes(command) || !option('root')) throw new Error('Usage: node scripts/dev.cjs start|stop|status|snapshot|screenshot|click|type --root <absolute environment directory> [--generation <id> --target <id> --ref <number> --text <text> --output <png>]');
  const root = path.resolve(option('root'));
  const descriptor = path.join(root, 'dev-control.json');
  if (command === 'start') {
    const workspace = path.resolve(__dirname, '../../..');
    desktopEnvironment({appData: path.dirname(root), root, workspace, packaged: false, channel: 'dev'});
    const built = path.resolve(__dirname, '../dist-dev/mac-arm64/Muteki Dev.app/Contents/MacOS/Muteki Dev');
    const packaged = option('executable') || (process.platform === 'darwin' && process.arch === 'arm64' && fs.existsSync(built) ? built : undefined);
    const executable = packaged || require('electron');
    const log = fs.openSync(path.join(root, 'logs', 'desktop.log'), 'a', 0o600);
    const env = {...process.env, MUTEKI_DESKTOP_WORKSPACE: workspace, MUTEKI_DESKTOP_NODE: process.execPath, MUTEKI_DESKTOP_REGISTER_PROTOCOL: '0'};
    for (const key of ['ELECTRON_RUN_AS_NODE', 'NODE_OPTIONS', 'NODE_PATH', 'MUTEKI_DESKTOP_URL', 'MUTEKI_DESKTOP_USER_DATA']) delete env[key];
    const child = spawn(executable, [...(packaged ? [] : [path.resolve(__dirname, '..')]), '--muteki-channel=dev', `--muteki-environment-root=${root}`, '--muteki-control'], {env, detached: true, stdio: ['ignore', log, log]});
    await new Promise((resolve, reject) => { child.once('spawn', resolve); child.once('error', reject); });
    fs.closeSync(log); child.unref();
    const deadline = Date.now() + 30000;
    while (Date.now() < deadline) {
      if (fs.existsSync(descriptor)) {
        const value = JSON.parse(fs.readFileSync(descriptor, 'utf8'));
        try {
          const response = await fetch(value.origin + '/status', {headers: {Authorization: `Bearer ${value.token}`}, signal: AbortSignal.timeout(2000)});
          if (response.ok) { const state = await response.json(); if (state.targets.some(target => target.url.startsWith('muteki-desktop://app/'))) { console.log(JSON.stringify(state)); return; } }
        } catch { /* the new process has not published its ready listener yet */ }
      }
      await new Promise(resolve => setTimeout(resolve, 200));
    }
    throw new Error(`Dev did not become ready; inspect ${path.join(root, 'logs', 'desktop.log')}`);
  }
  const value = JSON.parse(fs.readFileSync(descriptor, 'utf8'));
  if (command !== 'status' && (!option('generation') || (command !== 'stop' && !option('target')))) throw new Error('Read status first, then supply its generation and target');
  const body = command === 'status' ? undefined : JSON.stringify({action: command, generation: option('generation'), target: option('target'), ref: Number(option('ref')), text: option('text')});
  const response = await fetch(value.origin + (command === 'stop' ? '/stop' : body ? '/action' : '/status'), {method: body ? 'POST' : 'GET', headers: {Authorization: `Bearer ${value.token}`, 'Content-Type': 'application/json'}, body, signal: AbortSignal.timeout(30000)});
  const result = await response.json();
  if (!response.ok) throw Object.assign(new Error(result.message || result.code), {code: result.code});
  if (command === 'screenshot') {
    if (!option('output')) throw new Error('screenshot requires --output <png>');
    const file = path.resolve(option('output')); fs.writeFileSync(file, Buffer.from(result.png, 'base64')); console.log(JSON.stringify({path: file}));
  } else console.log(JSON.stringify(result));
}
main().catch(error => { console.error(JSON.stringify({code: error.code || 'dev.launch_failed', message: error.message})); process.exitCode = 1; });
